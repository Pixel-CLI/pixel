#!/usr/bin/env python3
"""Contract of .agents/skills/release/prepare.sh: the pull requests it lists.

Runs the real script inside a disposable workspace repository with stub
`cargo` and `gh` on PATH. The stub `gh` answers `pr list` with the pull
requests of the fixture when they are asked for on `main`, whatever the date
search says (GitHub's own search returned the previous prepare PR), and applies the script's `--jq` expression
with the real `jq`, so the expression is exercised too.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

PREPARE = Path(__file__).resolve().parent.parent / ".agents/skills/release/prepare.sh"
MANIFESTS = [
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    ".devin-plugin/plugin.json",
    ".qoder-plugin/plugin.json",
    "gemini-extension.json",
    "package.json",
]

GH = """#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
if os.environ.get("FIXTURE_GH_FAIL") == args[0]:
    sys.exit(1)
if args[:2] == ["pr", "list"]:
    # Pull requests merge into main: a listing on any other base (the retired
    # develop) finds none, so a script still asking for it lists nothing.
    base = args[args.index("--base") + 1] if "--base" in args else None
    repo = args[args.index("--repo") + 1] if "--repo" in args else "old/fixture"
    prs = json.loads(open(os.environ["FIXTURE_PRS"]).read()) if base == "main" and repo == "example/fixture" else []
    if os.environ.get("FIXTURE_EMPTY_SEARCH"):
        prs = []
    expr = args[args.index("--jq") + 1]
    out = subprocess.run(["jq", "-r", expr], input=json.dumps(prs),
                         capture_output=True, text=True, check=True)
    sys.stdout.write(out.stdout)
elif args[:2] == ["repo", "view"]:
    print("example/fixture")
elif args[:1] == ["api"]:
    sha = args[1].split("/")[-2]
    for pr in json.loads(open(os.environ["FIXTURE_PRS"]).read()):
        if pr["mergeCommit"]["oid"] == sha:
            print(pr["number"])
else:
    sys.exit(1)
"""


class PrepareContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-prepare-contract-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        fake = self.root / "fake"
        fake.mkdir()
        (fake / "gh").write_text(GH)
        (fake / "cargo").write_text("#!/bin/sh\nexit 0\n")
        for stub in fake.iterdir():
            stub.chmod(0o755)
        self.prs = self.root / "prs.json"
        self.env = {
            **os.environ,
            "PATH": str(fake) + os.pathsep + os.environ["PATH"],
            "FIXTURE_PRS": str(self.prs),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.com",
        }
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/a"]\n')
        self.write("crates/a/Cargo.toml", '[package]\nname = "a"\nversion = "0.1.0"\n')
        for manifest in MANIFESTS:
            self.write(manifest, '{\n  "name": "a",\n  "version": "0.1.0"\n}\n')
        self.write("plugin.yaml", "name: a\nversion: 0.1.0\n")
        self.write("scripts/gen-plugin-assets.sh", "#!/bin/sh\n")
        self.write("CHANGELOG.md", "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n")
        self.write("changelog.d/.gitkeep", "")
        self.git("init", "-q", "-b", "main")
        self.git("add", ".")
        self.git("commit", "-qm", "base")

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", str(self.repo), *args],
            env=self.env,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def merge_pr(self, branch, rel):
        """A pull request merged into main with a merge commit."""
        self.git("switch", "-q", "-c", branch)
        self.write(rel, branch + "\n")
        self.git("add", ".")
        self.git("commit", "-qm", branch)
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "Merge " + branch, branch)
        return self.git("rev-parse", "HEAD")

    def prepare(self):
        shutil.copy(PREPARE, self.repo / "prepare.sh")
        self.git("add", "prepare.sh")
        self.git("commit", "--allow-empty", "-qm", "script")
        return subprocess.run(
            ["sh", "prepare.sh", "0.2.0", "--date", "2026-02-01"],
            cwd=self.repo, env=self.env, capture_output=True, text=True, timeout=30,
        )

    def changelog(self):
        return (self.repo / "CHANGELOG.md").read_text()

    def fragments(self):
        return sorted(p.name for p in (self.repo / "changelog.d").glob("*.md"))

    def listed(self, stdout):
        head = "pull requests merged into main since v0.1.0"
        lines = stdout.splitlines()
        start = next(i for i, line in enumerate(lines) if line.startswith(head)) + 1
        block = []
        for line in lines[start:]:
            if not line.startswith("  "):
                break
            block.append(line.strip())
        return block

    def test_the_pull_request_the_tag_merged_is_not_listed_as_unreleased(self):
        released = self.merge_pr("release-0.1.0", "released.txt")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0", released)
        fixed = self.merge_pr("fix-thing", "fixed.txt")
        self.write("changelog.d/12-fix-thing.fixed.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")
        self.prs.write_text(json.dumps([
            {"number": 12, "title": "fix: thing", "mergeCommit": {"oid": fixed}},
            {"number": 11, "title": "release: prepare 0.1.0", "mergeCommit": {"oid": released}},
            # Merged on GitHub, merge commit not fetched into this clone.
            {"number": 13, "title": "fix: elsewhere", "mergeCommit": {"oid": "1" * 40}},
        ]))

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["#12 fix: thing"])

    def test_nothing_unreleased_says_none(self):
        released = self.merge_pr("release-0.1.0", "released.txt")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0", released)
        self.write("changelog.d/11-thing.fixed.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")
        self.prs.write_text(json.dumps([
            {"number": 11, "title": "release: prepare 0.1.0", "mergeCommit": {"oid": released}},
        ]))

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["(none)"])

    def test_a_transferred_repo_lists_only_prs_in_the_candidate(self):
        """The old remote can fetch but searches empty; later merges must not
        be described as part of a frozen candidate either."""
        self.git("remote", "add", "origin", "https://github.com/old/fixture.git")
        self.git("tag", "v0.1.0")
        self.write("changelog.d/12-fix.fixed.md", "**thing:** fixed.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix: thing (#12)")
        candidate = self.git("rev-parse", "HEAD")
        self.git("switch", "-q", "-c", "later")
        self.write("later.txt", "not in the release\n")
        self.git("add", ".")
        self.git("commit", "-qm", "feat: later (#13)")
        later = self.git("rev-parse", "HEAD")
        self.git("switch", "-q", "main")
        self.prs.write_text(json.dumps([
            {"number": 12, "title": "fix: thing", "mergeCommit": {"oid": candidate}},
            {"number": 13, "title": "feat: later", "mergeCommit": {"oid": later}},
        ]))
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["#12 fix: thing"])

    def test_incomplete_inventory_refuses_before_cutting(self):
        """An API failure, truncated search or false empty result must not
        consume fragments and leave an apparently complete release."""
        self.git("tag", "v0.1.0")
        self.write("changelog.d/12-fix.fixed.md", "**thing:** fixed.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix: thing (#12)")
        pr = {"number": 12, "title": "fix: thing",
              "mergeCommit": {"oid": self.git("rev-parse", "HEAD")}}
        before = self.changelog()
        for failure in ("repo", "pr", "api", "limit", "empty"):
            with self.subTest(failure=failure):
                self.prs.write_text(json.dumps([pr] * (1000 if failure == "limit" else 1)))
                self.env["FIXTURE_GH_FAIL"] = failure
                self.env["FIXTURE_EMPTY_SEARCH"] = "1" if failure == "empty" else ""
                result = self.prepare()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("incomplete release inventory", result.stderr)
                self.assertEqual(self.changelog(), before)
                self.assertEqual(self.fragments(), ["12-fix.fixed.md"])
                self.assertIn('version = "0.1.0"', (self.repo / "crates/a/Cargo.toml").read_text())

    def test_the_fragments_become_the_release_section_by_section(self):
        """One fragment per entry, filed under the heading its name names."""
        self.write("changelog.d/12-add-a-flag.added.md", "**thing:** `pixel thing --flag` is new.\n")
        self.write("changelog.d/13-fix-a-thing.fixed.md", "**thing:** `pixel thing` no longer breaks.\n")
        self.write("changelog.d/14-change-a-thing.changed.md", "**thing:** `pixel thing` says less.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragments")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        changelog = self.changelog()
        # A kept, empty Unreleased, then the release heading, then the
        # sections grouped, in the order the file has always used.
        self.assertTrue(changelog.startswith(
            "# Changelog\n\n## [Unreleased]\n\n## [0.2.0] - 2026-02-01\n"), changelog)
        self.assertEqual(
            [line for line in changelog.splitlines() if line.startswith("### ")],
            ["### Added", "### Changed", "### Fixed"],
        )
        for entry in ["- **thing:** `pixel thing --flag` is new.",
                      "- **thing:** `pixel thing` says less.",
                      "- **thing:** `pixel thing` no longer breaks."]:
            self.assertIn(entry + "\n", changelog)
        # The released entries leave the fragments behind them.
        self.assertEqual(self.fragments(), [])
        # The cut inserts a section, it does not swallow the ones after it:
        # Unreleased stays on top, the new section under it, the history last.
        self.assertLess(changelog.index("## [Unreleased]"), changelog.index("## [0.2.0]"))
        self.assertLess(
            changelog.index("## [0.2.0]"), changelog.index("## [0.1.0] - 2026-01-01")
        )

    def test_a_long_entry_keeps_every_line(self):
        """A wrapped entry keeps its continuation lines, indented under the bullet."""
        self.write("changelog.d/12-long.fixed.md", "**thing:** first line\ncontinued here\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("\n- **thing:** first line\n  continued here\n", self.changelog())

    def test_an_entry_without_a_reference_takes_the_link_of_its_squash_merge(self):
        """#550: the fragment is written once, before the number exists.

        Asking for the number in the fragment cost every pull request a second
        push only to rename the file; the squash merge that adds it to main
        ends with `(#<n>)`, so the cut appends the link there, at the end of
        the entry's last line, where a written one sits.
        """
        self.write("changelog.d/thing.fixed.md", "**thing:** it no longer breaks.\n")
        self.write("changelog.d/wrapped.added.md", "**thing:** a flag\nthat wraps.\n\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix(thing): stop breaking (#42)")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        changelog = self.changelog()
        self.assertIn(
            "\n- **thing:** it no longer breaks. "
            "([#42](https://github.com/Pixel-CLI/pixel/pull/42))\n", changelog)
        self.assertIn(
            "\n- **thing:** a flag\n  that wraps. "
            "([#42](https://github.com/Pixel-CLI/pixel/pull/42))\n", changelog)
        self.assertEqual(self.fragments(), [])

    def test_an_entry_merged_by_a_merge_commit_takes_that_pull_request(self):
        """A merge commit adds the file on main's first-parent line, under
        GitHub's `Merge pull request #<n>` subject, and a later pull request
        that only edits the entry does not take its credit."""
        self.git("switch", "-q", "-c", "topic")
        self.write("changelog.d/thing.fixed.md", "**thing:** it no longer breaks.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix(thing): stop breaking (#99)")
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "Merge pull request #7 from someone/topic", "topic")
        self.write("changelog.d/thing.fixed.md", "**thing:** it no longer breaks, at all.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "docs(changelog): reword (#8)")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "\n- **thing:** it no longer breaks, at all. "
            "([#7](https://github.com/Pixel-CLI/pixel/pull/7))\n", self.changelog())

    def test_a_written_reference_is_kept_as_written(self):
        """A link in the text or a number in the slug is the entry's own: the
        cut adds nothing to it, whatever the commit says."""
        self.write("changelog.d/12-numbered.fixed.md", "**thing:** numbered.\n")
        self.write("changelog.d/linked.fixed.md",
                   "**thing:** linked. ([#13](https://github.com/Pixel-CLI/pixel/pull/13))\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix(thing): both (#42)")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        changelog = self.changelog()
        self.assertIn("\n- **thing:** numbered.\n", changelog)
        self.assertIn(
            "\n- **thing:** linked. ([#13](https://github.com/Pixel-CLI/pixel/pull/13))\n",
            changelog)
        self.assertNotIn("/pull/42", changelog)

    def test_an_advisory_import_keeps_its_security_reference_without_a_public_pr(self):
        entry = "**security:** refuse hostile sidecars. ([GHSA-c9f5-vxc4-wjph](https://github.com/Pixel-CLI/pixel/security/advisories/GHSA-c9f5-vxc4-wjph))\n"
        self.write("changelog.d/trust.security.md", entry)
        self.git("add", ".")
        self.git("commit", "-qm", "Merge commit from fork")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("### Security\n- " + entry, self.changelog())
        self.assertNotIn("/pull/", self.changelog())
        self.assertEqual(self.fragments(), [])

    def test_only_a_complete_own_advisory_url_on_security_waives_the_pr_reference(self):
        own = "https://github.com/Pixel-CLI/pixel/security/advisories/GHSA-c9f5-vxc4-wjph"
        for section, link in (
            ("fixed", own),
            ("security", "GHSA-c9f5-vxc4-wjph"),
            ("security", own.replace("Pixel-CLI/pixel", "someone/else")),
            ("security", own + "extra"),
            ("security", own[:-1]),
        ):
            with self.subTest(section=section, link=link):
                name = "trust." + section + ".md"
                self.write("changelog.d/" + name, "**security:** fix. (" + link + ")\n")
                self.assert_refused_before_any_write(name, "no commit added it")
                (self.repo / "changelog.d" / name).unlink()

    def assert_refused_before_any_write(self, name, reason):
        """No entry ships without its reference: the cut stops on `name`,
        says why and how to fix it, and leaves the tree as it was."""
        before = self.changelog()

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(name + ": no pull request referenced, and " + reason, result.stderr)
        self.assertIn("https://github.com/Pixel-CLI/pixel/pull/<number>", result.stderr)
        self.assertEqual(self.changelog(), before)
        self.assertIn(name, self.fragments())
        self.assertIn('version = "0.1.0"', (self.repo / "crates/a/Cargo.toml").read_text())
        return result

    def test_an_entry_pushed_straight_to_main_is_refused_before_any_write(self):
        """A commit that names no pull request leaves the cut nothing to link."""
        self.write("changelog.d/pushed.fixed.md", "**thing:** it no longer breaks.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fix(thing): pushed straight to main")
        result = self.assert_refused_before_any_write(
            "pushed.fixed.md", "the commit that added it (")
        self.assertIn("fix(thing): pushed straight to main) names none", result.stderr)

    def test_an_entry_no_commit_added_is_refused_before_any_write(self):
        """An uncommitted fragment was never merged by any pull request."""
        self.write("changelog.d/loose.fixed.md", "**thing:** it no longer breaks.\n")
        self.assert_refused_before_any_write("loose.fixed.md", "no commit added it")

    def test_the_highlights_lead_the_released_section(self):
        """The release narrative, once per release instead of once per entry.

        With nowhere to put "what this release is about", every entry carries
        a sentence of it: that is how 0.4.0 reached a median bullet of 715
        bytes. It goes above the sections because release.yml cuts the GitHub
        release body from the version heading to the next `## `, so the body
        opens on it.
        """
        self.write("changelog.d/_highlights.md",
                   "This release is about the changelog.\n\n### Highlights\n- one thing.\n")
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragments")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "## [0.2.0] - 2026-02-01\n\nThis release is about the changelog.\n"
            "\n### Highlights\n- one thing.\n\n### Fixed\n- **thing:** thing\n",
            self.changelog(),
        )
        # It is consumed by the cut like any fragment: left behind, it would
        # lead the next release with the previous release's narrative.
        self.assertEqual(self.fragments(), [])
        self.assertIn("led by changelog.d/_highlights.md", result.stdout)

    def test_the_highlights_are_not_counted_as_an_entry(self):
        """A chapeau is not a changelog entry: alone, there is nothing to release.

        Counted as one, it would let a release be cut whose section holds a
        narrative and no bullet -- and be filed under a section its name does
        not have.
        """
        self.write("changelog.d/_highlights.md", "Only a narrative.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "highlights only")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_a_fragment_without_a_known_section_is_refused(self):
        self.write("changelog.d/12-thing.fized.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)
        self.assertIn("fixed", result.stderr)

    def test_a_nameless_fragment_is_refused(self):
        self.write("changelog.d/12-thing.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)

    def test_an_empty_fragment_is_refused(self):
        self.write("changelog.d/12-thing.fixed.md", "")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is empty", result.stderr)

    def test_a_whitespace_only_fragment_is_refused(self):
        """`-s` is not enough: a file of newlines would file a bare `- `."""
        for name, body in (("12-newline.fixed.md", "\n"),
                           ("13-spaces.fixed.md", "   \n"),
                           ("14-blanks.fixed.md", "\n\n\n")):
            with self.subTest(name=name):
                self.write("changelog.d/" + name, body)
                self.git("add", ".")
                self.git("commit", "-qm", "fragment")

                result = self.prepare()

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("is empty", result.stderr)
                self.git("reset", "-q", "--hard", "HEAD~1")

    def test_a_fragment_whose_first_line_is_blank_is_refused(self):
        """The first line is the bullet; a blank one files `- ` and an indent."""
        self.write("changelog.d/12-thing.fixed.md", "\nthe entry on the second line\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("starts with a blank line", result.stderr)

    def test_an_entry_left_under_unreleased_is_refused(self):
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
        self.write("CHANGELOG.md",
                   "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- written straight into the file\n\n## [0.1.0] - 2026-01-01\n")
        self.git("add", ".")
        self.git("commit", "-qm", "stray")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bullet(s) still under ## [Unreleased]", result.stderr)
        self.assertIn("entries live in changelog.d/", result.stderr)
        # Refused before any write: the stray line is still there, and so is
        # the fragment nobody cut.
        self.assertIn("- written straight into the file", self.changelog())
        self.assertEqual(self.fragments(), ["12-thing.fixed.md"])

    def test_no_fragment_means_nothing_to_release(self):
        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_a_version_check_still_runs_before_the_fragments_are_touched(self):
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
        self.write("CHANGELOG.md",
                   "# Changelog\n\n## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n## [0.1.0] - 2026-01-01\n")
        self.git("add", ".")
        self.git("commit", "-qm", "heading")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already has a ## [0.2.0] heading", result.stderr)
        self.assertEqual(self.fragments(), ["12-thing.fixed.md"])

    def test_a_fragment_written_before_the_cut_waits_for_the_next_release(self):
        """The point of the directory, and what CHANGELOG.md could not do.

        A branch cut before the release carries an entry that does not belong
        to it. Written straight into CHANGELOG.md, a three-way merge puts that
        entry under the heading of the release that has just shipped -- no
        conflict, and a rebase does the same -- so the entry is published in a
        release it was never part of. A fragment is simply not in the cut.
        """
        self.write("changelog.d/12-shipped.fixed.md", "**thing:** shipped before the cut\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment to release")
        self.git("switch", "-q", "-c", "in-flight")
        self.write("changelog.d/13-later.fixed.md", "**thing:** for the release after this one\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment for the next release")

        self.git("switch", "-q", "main")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # The script never commits: the maintainer does, reviewing the diff.
        self.git("add", "-A")
        self.git("commit", "-qm", "release: prepare 0.2.0")
        self.git("merge", "-q", "--no-ff", "-m", "Merge in-flight", "in-flight")

        changelog = self.changelog()
        released = changelog.split("## [0.2.0]")[1].split("## [0.1.0]")[0]
        self.assertIn("shipped before the cut", released)
        self.assertNotIn("for the release after this one", released)
        # Still a fragment, waiting for the release that will fold it in.
        self.assertEqual(self.fragments(), ["13-later.fixed.md"])
        self.assertIn("## [Unreleased]", changelog)


class FragmentContract(unittest.TestCase):
    """`prepare.sh --check` accepts what this repository actually ships.

    It is what turns a mistyped section in a pull request into a red check on
    that pull request, instead of a release that refuses to tag.
    """

    ROOT = PREPARE.parent.parent.parent.parent

    def run_check(self, root):
        return subprocess.run(
            ["sh", str(PREPARE), "--check"],
            cwd=root, capture_output=True, text=True, timeout=30,
        )

    CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n"

    def make_repo(self, fragments, changelog=None):
        """A bare repository holding only what --check reads."""
        tmp = tempfile.TemporaryDirectory(prefix="pixel-prepare-check-")
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        (root / "changelog.d").mkdir()
        for name, text in fragments.items():
            (root / "changelog.d" / name).write_text(text)
        (root / "CHANGELOG.md").write_text(self.CHANGELOG if changelog is None else changelog)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=root, check=True)
        return root

    def test_the_repository_fragments_are_well_formed(self):
        result = self.run_check(self.ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("well formed", result.stdout)
        self.assertIn("## [Unreleased] empty", result.stdout)

    def test_check_accepts_the_empty_directory_a_release_leaves_behind(self):
        """A release pull request is the one that empties changelog.d/.

        The cut deletes every fragment, so the commit `--check` runs on has an
        empty directory. While the no-fragment refusal sat above the `--check`
        return it failed that pull request -- the release of 0.4.0 went red on
        its own preparation -- and the only way to green it was to stop cutting
        the release or to write a fragment nobody had an entry for.
        """
        root = self.make_repo({})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("well formed", result.stdout)
        self.assertIn("## [Unreleased] empty", result.stdout)

    def test_the_cut_still_refuses_the_empty_directory_check_accepts(self):
        """`--check` accepting it must not make the cut accept it too.

        Tagging a version whose changelog section would be empty is the thing
        the refusal exists for; only the validator had to stop sharing it.
        """
        root = self.make_repo({})
        result = subprocess.run(
            ["sh", str(PREPARE), "9.9.9"],
            cwd=root, capture_output=True, text=True, timeout=30,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_check_writes_nothing(self):
        before = {p: p.read_bytes() for p in sorted(self.ROOT.glob("changelog.d/*.md"))}
        self.run_check(self.ROOT)
        self.assertEqual(
            before,
            {p: p.read_bytes() for p in sorted(self.ROOT.glob("changelog.d/*.md"))},
        )

    def test_check_refuses_a_repository_whose_fragments_are_misnamed(self):
        root = self.make_repo({"thing.fized.md": "**thing:** thing\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)

    def test_check_refuses_a_whitespace_only_fragment(self):
        root = self.make_repo({"12-thing.fixed.md": "\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is empty", result.stderr)

    def entry(self, length, prefix="**thing:** ", suffix=" (#12)"):
        """An entry of exactly `length` bytes, scope and link included."""
        body = "x" * (length - len(prefix) - len(suffix))
        return prefix + body + suffix + "\n"

    def test_check_refuses_an_entry_that_does_not_open_on_its_scope(self):
        """The scope is what makes a released section scannable.

        0.4.0 shipped twelve bullets with none, so finding the entry about a
        given command means reading every one of them to the first backtick.
        """
        root = self.make_repo({"12-thing.fixed.md": "`pixel thing` no longer breaks.\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("open the entry with the scope it changes", result.stderr)
        self.assertIn("**graph:**", result.stderr)

    def test_check_accepts_a_scope_naming_more_than_one_area(self):
        """A change landing in two places still has one scope line."""
        root = self.make_repo({"12-thing.fixed.md": "**graph, daemon:** it no longer breaks. (#12)\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_check_refuses_an_entry_over_the_cap(self):
        """The cap is the whole point: the reasoning belongs to the pull request.

        The fragment this gate was written for ran 1428 bytes in one paragraph,
        most of it arguing for the threshold it picked -- an argument the pull
        request already carried, and that a reader of the changelog is not
        looking for.
        """
        root = self.make_repo({"12-thing.fixed.md": self.entry(901)})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("901 bytes, over the 900 cap", result.stderr)
        self.assertIn("leave the reasoning to the pull request", result.stderr)

    def test_the_cap_measures_the_entry_and_not_its_first_line(self):
        """A wrapped fragment is one entry; the cut reflows it under one bullet.

        Measuring the first line alone would let the same prose through by
        pressing the return key.
        """
        wrapped = self.entry(901).replace("xxxxxxxxxx", "xxxxx\nxxxxx", 1)
        self.assertIn("\n", wrapped.strip())
        root = self.make_repo({"12-thing.fixed.md": wrapped})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("over the 900 cap", result.stderr)

    def test_an_entry_over_the_style_length_warns_without_refusing(self):
        """Between the two limits the entry ships, and the author is told.

        A hard cap alone would make 900 bytes the target; the warning is what
        keeps 500 the one, without refusing the entry that genuinely carries a
        before/after measurement.
        """
        root = self.make_repo({"12-thing.fixed.md": self.entry(501)})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("501 bytes, over the 500", result.stderr)
        self.assertIn("not a refusal", result.stderr)
        self.assertIn("well formed", result.stdout)

    def test_an_entry_referencing_no_pull_request_waits_for_its_merge(self):
        """The pull request's own run cannot know a number the merge gives.

        It used to refuse here, which cost every pull request a second push
        only to rename its fragment (#550). The cut appends the link from the
        merge commit and refuses an entry it cannot link
        (`PrepareContract`), so the reference is still never left out; the
        check only says which entries will take it.
        """
        root = self.make_repo({
            "thing.fixed.md": "**thing:** it no longer breaks.\n",
            "other.added.md": "**thing:** a flag.\n",
            "12-numbered.fixed.md": "**thing:** numbered.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("no pull request referenced", result.stderr)
        self.assertIn(
            "2 entries take their pull request link from the commit that merges it at the cut",
            result.stdout)
        self.assertIn("3 fragment(s) under changelog.d/, all well formed", result.stdout)

    def test_an_issue_number_is_not_a_pull_request_reference(self):
        """`#42` alone may be an issue: the text counts only with the URL, so
        the entry still waits for the link of its merge."""
        root = self.make_repo({"thing.fixed.md": "**thing:** it no longer breaks (fixes issue #42).\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 entry takes its pull request link", result.stdout)

    def test_a_link_in_the_entry_references_the_pull_request(self):
        """The link alone is enough, whatever the slug."""
        root = self.make_repo({
            "thing.fixed.md": "**thing:** it no longer breaks. ([#12](https://github.com/Pixel-CLI/pixel/pull/12))\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("pull request link from the commit", result.stdout)

    def test_a_number_first_in_the_slug_references_the_pull_request(self):
        """The convention the directory already had counts as the reference."""
        root = self.make_repo({"12-thing.fixed.md": "**thing:** it no longer breaks.\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("pull request link from the commit", result.stdout)

    def test_check_reports_the_highlights_next_to_the_entries(self):
        root = self.make_repo({
            "_highlights.md": "A narrative.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 fragment(s) under changelog.d/ plus _highlights.md", result.stdout)

    def test_check_refuses_a_misspelt_highlights_file(self):
        """Skipping it silently would drop the chapeau from its own release.

        `_highlights.md` is the one underscore name the directory takes, so
        anything else with that prefix is a typo of it, not a new convention.
        """
        root = self.make_repo({
            "_highlight.md": "A narrative.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the only underscore-named file changelog.d/ takes is _highlights.md",
                      result.stderr)

    def test_check_refuses_a_heading_that_would_end_the_release_body(self):
        """release.yml cuts the body from `## [x.y.z]` to the next `## `.

        A `##` heading inside the chapeau truncates the release notes there,
        dropping every section under it -- the entries included.
        """
        for heading in ("## Highlights", "# Highlights"):
            with self.subTest(heading=heading):
                root = self.make_repo({
                    "_highlights.md": "A narrative.\n\n" + heading + "\n- one thing.\n",
                    "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
                })
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("use `###` and below", result.stderr)

    def test_check_accepts_a_third_level_heading_in_the_highlights(self):
        root = self.make_repo({
            "_highlights.md": "A narrative.\n\n### Highlights\n- one thing.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_check_refuses_highlights_over_the_cap(self):
        """The cap is what stops the prose moving from the entries into here."""
        root = self.make_repo({
            "_highlights.md": "x" * 2001 + "\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("2001 bytes, over the 2000 cap", result.stderr)

    def test_check_refuses_an_empty_highlights_file(self):
        """Absent is a valid release; present and empty is a forgotten one."""
        root = self.make_repo({
            "_highlights.md": "\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("_highlights.md is empty", result.stderr)

    def test_the_highlights_are_not_held_to_the_entry_style(self):
        """It is a paragraph, not a bullet: no scope prefix, no 500-byte aim."""
        root = self.make_repo({
            "_highlights.md": "A narrative of " + "x" * 600 + ".\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # Seen, and held to its own limits rather than the entry's.
        self.assertIn("plus _highlights.md", result.stdout)
        self.assertNotIn("open the entry with the scope", result.stderr)
        self.assertNotIn("the style aims at", result.stderr)

    def test_check_does_not_report_unreleased_empty_without_looking(self):
        """The success message claims something; it has to have checked it.

        `--check` used to return before the stray-bullet test, so a tree that
        release preparation refuses was reported as well formed.
        """
        root = self.make_repo(
            {"12-thing.fixed.md": "**thing:** thing\n"},
            changelog="# Changelog\n\n## [Unreleased]\n\n### Fixed\n- stray\n\n## [0.1.0] - 2026-01-01\n",
        )
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bullet(s) still under ## [Unreleased]", result.stderr)
        self.assertNotIn("all well formed", result.stdout)


if __name__ == "__main__":
    unittest.main()
