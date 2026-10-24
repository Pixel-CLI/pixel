#!/usr/bin/env python3
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT

"""Tests for scripts/release/check-release.py."""

import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "check_release", Path(__file__).resolve().parent / "release" / "check-release.py"
)
check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check)

CLI = '[package]\nname = "pixel"\nversion = "0.2.3"\nedition.workspace = true\n\n[[bin]]\nname = "pixel"\n'
LOCK = (
    'version = 4\n\n[[package]]\nname = "pixel"\nversion = "0.2.3"\ndependencies = [\n "serde",\n]\n\n'
    '[[package]]\nname = "pixel-io"\nversion = "0.2.2"\n\n'
    '[[package]]\nname = "serde"\nversion = "1.0.0"\nsource = "registry"\n'
)


class Normalize(unittest.TestCase):
    def test_accepts_the_tag_shapes_and_rejects_the_rest(self):
        for text, want in [
            ("1.2.3", "1.2.3"),
            ("v1.2.3", "1.2.3"),
            ("refs/tags/v1.2.3", "1.2.3"),
            (" v10.20.30 ", "10.20.30"),
            ("1.2.3-rc.1", "1.2.3-rc.1"),
        ]:
            self.assertEqual(check.normalize_version(text), want, text)
        for bad in ["", "v", "1.2", "1.2.3.4", "1.x.3", "vv1.2.3", "1..3", "1.2.3a", "١.٢.٣"]:
            self.assertIsNone(check.normalize_version(bad), bad)


class Manifests(unittest.TestCase):
    def test_the_package_table_alone_names_and_versions_a_crate(self):
        self.assertEqual(check.package_name_version(CLI), ("pixel", "0.2.3"))
        self.assertIsNone(
            check.package_name_version('[dependencies]\nname = "x"\nversion = "9.9.9"\n')
        )
        self.assertIsNone(check.package_name_version('[package]\nname = "x"\n'))
        self.assertIsNone(
            check.package_name_version('[package]\nname = "x"\nversion.workspace = true\n')
        )
        self.assertIsNone(check.package_name_version("not toml ["))

    def test_workspace_members_expand_globs_and_keep_plain_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ("a", "b"):
                (root / "crates" / name).mkdir(parents=True)
                (root / "crates" / name / "Cargo.toml").write_text("")
            (root / "crates" / "notacrate").mkdir()
            manifest = '[workspace]\nmembers = ["crates/*", "tools/x"]\n'
            self.assertEqual(
                check.workspace_members(manifest, root), ["crates/a", "crates/b", "tools/x"]
            )
            self.assertEqual(check.workspace_members('[workspace]\nresolver = "3"\n', root), [])

    def test_the_lock_lists_every_package(self):
        self.assertEqual(
            check.lock_packages(LOCK),
            [("pixel", "0.2.3"), ("pixel-io", "0.2.2"), ("serde", "1.0.0")],
        )


class Checks(unittest.TestCase):
    def test_the_cli_version_must_equal_the_tag(self):
        self.assertEqual(
            check.check_cli_version(CLI, "0.2.3"), ("cli-version", True, "pixel 0.2.3 matches")
        )
        name, ok, detail = check.check_cli_version(CLI, "0.2.4")
        self.assertFalse(ok)
        self.assertIn("is 0.2.3, tag says 0.2.4", detail)
        self.assertFalse(check.check_cli_version("[dependencies]\n", "0.2.3")[1])

    def test_the_lock_check_flags_stale_and_missing_members_only(self):
        fresh = check.check_lock(LOCK, [("pixel", "0.2.3"), ("pixel-io", "0.2.2")])
        self.assertEqual(fresh, ("cargo-lock", True, "2 workspace members at their manifest version"))
        _, ok, detail = check.check_lock(LOCK, [("pixel", "0.2.4"), ("pixel-new", "0.1.0")])
        self.assertFalse(ok)
        self.assertIn("pixel 0.2.3 (manifest 0.2.4)", detail)
        self.assertIn("pixel-new missing from Cargo.lock", detail)
        self.assertNotIn("serde", detail)

    def test_the_changelog_needs_the_heading_and_an_empty_unreleased(self):
        good = "## [Unreleased]\n\n## [0.2.3] - 2026-10-22\n\n### Added\n\n- **x:** y\n"
        self.assertEqual(
            check.check_changelog(good, "0.2.3"),
            ("changelog", True, "## [0.2.3] present, Unreleased empty"),
        )
        left = "## [Unreleased]\n\n- **x:** forgotten\n\n## [0.2.3] - 2026-10-22\n"
        _, ok, detail = check.check_changelog(left, "0.2.3")
        self.assertFalse(ok)
        self.assertIn("1 entry still under ## [Unreleased]", detail)
        uncut = "## [Unreleased]\n\n- a\n- b\n"
        _, ok, detail = check.check_changelog(uncut, "0.2.3")
        self.assertFalse(ok)
        self.assertIn("no ## [0.2.3] heading; 2 entries under ## [Unreleased]", detail)
        _, ok, detail = check.check_changelog("## [Unreleased]\n", "0.2.3")
        self.assertFalse(ok)
        self.assertEqual(detail, "no ## [0.2.3] heading in CHANGELOG.md")
        # A longer version is not the heading of the shorter one.
        _, ok, _ = check.check_changelog("## [Unreleased]\n\n## [0.2.30] - d\n", "0.2.3")
        self.assertFalse(ok)


class Run(unittest.TestCase):
    def repo(self, root, cli=CLI, lock=LOCK, changelog=None):
        (root / "crates" / "pixel").mkdir(parents=True)
        (root / "crates" / "pixel-io").mkdir()
        (root / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/pixel", "crates/pixel-io"]\n')
        (root / "crates" / "pixel" / "Cargo.toml").write_text(cli)
        (root / "crates" / "pixel-io" / "Cargo.toml").write_text(
            '[package]\nname = "pixel-io"\nversion = "0.2.2"\n'
        )
        (root / "Cargo.lock").write_text(lock)
        (root / "CHANGELOG.md").write_text(
            changelog or "## [Unreleased]\n\n## [0.2.3] - 2026-10-22\n\n- done\n"
        )

    def test_a_consistent_repository_passes_and_renders_every_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.repo(Path(tmp))
            version, checks = check.run(tmp, "refs/tags/v0.2.3")
            self.assertEqual(version, "0.2.3")
            self.assertTrue(all(ok for _, ok, _ in checks))
            text = check.render(version, checks)
            self.assertIn("[ok  ] cargo-lock:", text)
            self.assertTrue(text.endswith("check-release: all checks passed\n"))
            self.assertEqual(check.main(["v0.2.3", "--repo", tmp]), 0)

    def test_a_stale_lock_fails_the_report_with_exit_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.repo(Path(tmp), lock=LOCK.replace('version = "0.2.3"', 'version = "0.2.2"', 1))
            _, checks = check.run(tmp, "0.2.3")
            self.assertEqual([ok for _, ok, _ in checks], [True, False, True])
            self.assertTrue(check.render("0.2.3", checks).endswith("check-release: FAILED\n"))
            self.assertEqual(check.main(["0.2.3", "--repo", tmp]), 1)

    def test_a_malformed_version_or_a_missing_file_is_exit_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(check.main(["1.2", "--repo", tmp]), 2)
            self.assertEqual(check.main(["1.2.3", "--repo", tmp]), 2)


if __name__ == "__main__":
    unittest.main()
