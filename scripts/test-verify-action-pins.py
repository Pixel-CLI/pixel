#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/verify-action-pins.py.

The check exists for one failure: a `uses:` that runs whatever a movable
tag points to on the day of the run, in a workflow that holds a publishing
token or an OIDC signing identity. So the cases that matter are the ones it
must refuse -- a tag, an abbreviated SHA, a form it cannot read -- and the
repository's own workflows, which must pass as they stand.
"""

import importlib.util
from pathlib import Path
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("verify_action_pins", REPO / "scripts/verify-action-pins.py")
pins = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pins)

SHA = "3d3c42e5aac5ba805825da76410c181273ba90b1"


class CheckLine(unittest.TestCase):
    def assert_refused(self, line: str, fragment: str) -> None:
        reason = pins.check_line(line)
        self.assertIsNotNone(reason, f"accepted: {line!r}")
        self.assertIn(fragment, reason)

    def test_pinned_step_with_version_comment_passes(self):
        self.assertIsNone(pins.check_line(f"      - uses: actions/checkout@{SHA} # v7"))
        self.assertIsNone(pins.check_line(f"        uses: dtolnay/rust-toolchain@{SHA} # stable"))

    def test_pinned_reusable_workflow_of_another_repo_passes(self):
        self.assertIsNone(pins.check_line(f"    uses: org/repo/.github/workflows/build.yml@{SHA} # v1.2.0"))

    def test_local_action_and_workflow_pass(self):
        self.assertIsNone(pins.check_line("    uses: ./.github/workflows/release-prepare-scope.yml"))
        self.assertIsNone(pins.check_line("      - uses: ./.github/actions/setup"))

    def test_docker_image_by_digest_passes_by_tag_fails(self):
        self.assertIsNone(pins.check_line("      - uses: docker://alpine@sha256:" + "a" * 64))
        self.assert_refused("      - uses: docker://alpine:3.20", "sha256")

    def test_tag_or_branch_is_refused(self):
        self.assert_refused("      - uses: actions/checkout@v7", "`@v7` is not a full 40-hex commit SHA")
        self.assert_refused("      - uses: actions/checkout@main # main", "not a full 40-hex")

    def test_abbreviated_or_uppercase_sha_is_refused(self):
        self.assert_refused(f"      - uses: actions/checkout@{SHA[:12]} # v7", "not a full 40-hex")
        self.assert_refused(f"      - uses: actions/checkout@{SHA.upper()} # v7", "not a full 40-hex")

    def test_sha_without_comment_is_refused(self):
        # Dependabot reads the comment to name the bump; a bare SHA is also
        # unreviewable at a glance.
        self.assert_refused(f"      - uses: actions/checkout@{SHA}", "comment")
        self.assert_refused(f"      - uses: actions/checkout@{SHA} #", "comment")

    def test_forms_the_line_reader_cannot_parse_fail_closed(self):
        # Valid YAML, all of it: skipping these would skip a floating tag.
        for line in (
            f'      - "uses": actions/checkout@v7',
            f"      - 'uses' : actions/checkout@v7",
            f"      - {{uses: actions/checkout@{SHA}}}",
            f'      - uses: "actions/checkout@{SHA}" # v7',
            f"      - uses: actions/checkout@{SHA} v7",
        ):
            with self.subTest(line=line):
                self.assertIsNotNone(pins.check_line(line), f"accepted: {line!r}")

    def test_free_text_after_the_comment_marker_is_the_comment(self):
        self.assertIsNone(pins.check_line(f"      - uses: actions/checkout@{SHA} # v7.0.1 (Node 24)"))

    def test_non_uses_lines_are_ignored(self):
        for line in ("      - name: uses the cache", "        run: echo uses: nothing", "  reuses: x"):
            with self.subTest(line=line):
                self.assertIsNone(pins.check_line(line))


class Repository(unittest.TestCase):
    def test_this_repository_passes(self):
        self.assertEqual(pins.offenders(REPO), [])
        self.assertEqual(pins.main(["verify-action-pins.py", str(REPO)]), 0)

    def test_every_workflow_extension_and_local_action_is_scanned(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".github/workflows").mkdir(parents=True)
            (root / ".github/actions/setup").mkdir(parents=True)
            (root / ".github/workflows/a.yml").write_text(f"    - uses: actions/checkout@{SHA} # v7\n")
            (root / ".github/workflows/b.yaml").write_text("    - uses: actions/checkout@v7\n")
            (root / ".github/actions/setup/action.yml").write_text("    - uses: actions/cache@v4\n")
            self.assertEqual(
                [line.split(":")[0] + ":" + line.split(":")[1] for line in pins.offenders(root)],
                [".github/actions/setup/action.yml:1", ".github/workflows/b.yaml:1"],
            )
            self.assertEqual(pins.main(["verify-action-pins.py", str(root)]), 1)

    def test_no_workflow_at_all_is_a_failure_not_a_pass(self):
        # A wrong path would otherwise check nothing and report success.
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(pins.main(["verify-action-pins.py", tmp]), 1)


if __name__ == "__main__":
    unittest.main()
