#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Cargo library root selection fixtures for the overview probe."""

import importlib.machinery
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path


PROBE = Path(__file__).resolve().parents[1] / "probe"
LOADER = importlib.machinery.SourceFileLoader("overview_probe", str(PROBE))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
OVERVIEW = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(OVERVIEW)


class CrateSourceTests(unittest.TestCase):
    def test_default_library_root_wins_over_private_module_documentation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "member"
            (crate / "src").mkdir(parents=True)
            public = crate / "src/lib.rs"
            private = crate / "src/internal.rs"
            public.write_text("//! The public crate facade.\npub struct Api;\n")
            private.write_text("//! Private implementation details.\npub(crate) struct Internal;\n")
            relative = {"member/src/lib.rs", "member/src/internal.rs"}

            path, source = OVERVIEW.crate_source(
                str(root), "member", {"package": {"name": "member"}}, relative
            )

        self.assertEqual(path, "member/src/lib.rs")
        self.assertIn("pub struct Api", source)
        self.assertNotIn("Internal", source)

    def test_manifest_library_path_overrides_the_default(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            crate = root / "member"
            (crate / "src").mkdir(parents=True)
            custom = crate / "src/api.rs"
            private = crate / "src/lib.rs"
            custom.write_text("//! The configured library target.\npub struct Api;\n")
            private.write_text("//! This file is not the selected target.\npub struct Private;\n")
            relative = {"member/src/api.rs", "member/src/lib.rs"}

            path, source = OVERVIEW.crate_source(
                str(root), "member", {"lib": {"path": "src/api.rs"}}, relative
            )

        self.assertEqual(path, "member/src/api.rs")
        self.assertIn("pub struct Api", source)
        self.assertNotIn("Private", source)

    def test_tracked_fifo_is_rejected_without_blocking(self):
        if not hasattr(os, "mkfifo"):
            self.skipTest("FIFO fixtures require a Unix-like platform")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            member = root / "member"
            member.mkdir()
            fifo = member / "pipe"
            os.mkfifo(fifo)

            content = OVERVIEW.safe_read(str(root), "member/pipe", {"member/pipe"})

        self.assertIsNone(content)


if __name__ == "__main__":
    unittest.main()
