#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/clean.sh: what it reclaims, and what it must not touch.

Every case runs the real script against a disposable git repository that has a
second worktree, because the scopes cover every worktree and not only the one
the script runs from. The assertions are on the filesystem afterwards, not on
the report.

The guard cases are the reason this file exists. `clean.sh` builds its removal
list from names -- `target`, `.pixel`, `mutants.out*` -- and an `rm -rf` fed by
a name list is one stale pattern away from costing a tracked file, or a tree
that only a symlink pointed at. Both are unrecoverable, so both are pinned
here. The third guard, a candidate outside every worktree and outside the pixel
cache, is unreachable from the collection as written: it is the net under a
future scope, and `test_cargo_target_dir_replaces_the_per_worktree_targets`
covers the one path that legitimately leaves the tree.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

CLEAN = Path(__file__).with_name("clean.sh")

# The fixture's ignore rules, deliberately not a copy of the repository's:
# DEPENDENCIES.md is missing from it so that a name clean.sh collects is, in
# this repository, a file git tracks.
IGNORES = """\
/target
.pixel/
/mutants.out*
coverage*.txt
*.lcov
__pycache__/
**/node_modules/
.gitnexus/
stacklit.json
stacklit.html
"""

STUB_PIXEL = """\
#!/bin/sh
echo "$*" >> "$PIXEL_LOG"
"""


class CleanContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-clean-contract-")
        self.addCleanup(self.tmp.cleanup)
        # macOS hides its temp dir behind a /var -> /private/var symlink, and
        # the script matches paths it built against paths git printed.
        self.root = Path(self.tmp.name).resolve()
        self.repo = self.root / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(CLEAN, self.repo / "scripts/clean.sh")
        (self.repo / ".gitignore").write_text(IGNORES)
        (self.repo / "src").mkdir()
        (self.repo / "src/lib.rs").write_text("pub fn a() {}\n")
        self.git("init", "-q", "-b", "main")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.worktree = self.root / "wt"
        self.git("worktree", "add", "-q", str(self.worktree), "-b", "second")

        stub = self.root / "bin"
        stub.mkdir()
        (stub / "pixel").write_text(STUB_PIXEL)
        (stub / "pixel").chmod(0o755)
        self.pixel_log = self.root / "pixel.log"
        self.env = {
            k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"
        }
        self.env.update({
            "HOME": str(self.root / "home"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            # Never the installed binary: a case that reaches `daemon stop`
            # must not reach a daemon of the developer's own checkouts.
            "PIXEL_BIN": str(stub / "pixel"),
            "PIXEL_LOG": str(self.pixel_log),
            "GIT_CONFIG_GLOBAL": "/dev/null",
        })

    def git(self, *args):
        subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, capture_output=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
                 "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
                 "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"},
        )

    def clean(self, *args, **extra_env):
        return subprocess.run(
            ["sh", str(self.repo / "scripts/clean.sh"), *args],
            cwd=self.repo, env={**self.env, **extra_env},
            capture_output=True, text=True, timeout=120,
        )

    def build_output(self, worktree: Path):
        """A worktree as a build and an index leave it."""
        (worktree / "target/debug").mkdir(parents=True, exist_ok=True)
        (worktree / "target/debug/blob").write_bytes(b"x" * 4096)
        (worktree / ".pixel").mkdir(exist_ok=True)
        (worktree / ".pixel/base.shard").write_bytes(b"y" * 4096)

    def logged_pixel_calls(self):
        if not self.pixel_log.exists():
            return []
        return self.pixel_log.read_text().splitlines()

    def test_default_scope_removes_build_output_and_keeps_the_index(self):
        """An index costs minutes to rebuild; a build costs one cargo run.

        `just clean` is the recipe reached for without thinking, so its scope
        is the cheap one. Losing .pixel to it would be a trap, not a cleanup.
        """
        self.build_output(self.repo)
        result = self.clean()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.repo / "target").exists())
        self.assertTrue((self.repo / ".pixel/base.shard").exists())
        self.assertIn("reclaimed", result.stdout)

    def test_every_worktree_is_cleaned_not_only_the_one_it_runs_from(self):
        """Four worktrees carry four workspace builds; that is where the disk went.

        A scope that stopped at the current worktree would leave the largest
        share of the reclaimable bytes behind and read as if it had not.
        """
        self.build_output(self.repo)
        self.build_output(self.worktree)
        result = self.clean("build")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.repo / "target").exists())
        self.assertFalse((self.worktree / "target").exists())

    def test_dry_run_reports_a_total_and_removes_nothing(self):
        """`just disk` is the look-before-you-leap recipe.

        It is also what tells a reader whether a scope is worth running, so it
        has to name the total it would reclaim and leave every byte in place.
        """
        self.build_output(self.repo)
        self.build_output(self.worktree)
        result = self.clean("all", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("would be reclaimed", result.stdout)
        self.assertIn(str(self.repo / "target"), result.stdout)
        self.assertTrue((self.repo / "target/debug/blob").exists())
        self.assertTrue((self.worktree / ".pixel/base.shard").exists())
        self.assertEqual(self.logged_pixel_calls(), [])

    def test_a_file_git_tracks_is_refused_even_when_its_name_matches(self):
        """git's ignore rules are the authority on what is disposable.

        DEPENDENCIES.md is on the collection list because a bench run writes
        one, and this repository ignores it. Somewhere that rule is missing,
        the same name is a file somebody wrote: the run stops red with the path
        named rather than removing it.
        """
        (self.repo / "DEPENDENCIES.md").write_text("hand-written\n")
        self.git("add", "DEPENDENCIES.md")
        self.git("commit", "-qm", "tracked dependencies doc")
        result = self.clean("build")
        self.assertEqual(result.returncode, 1)
        self.assertIn("git does not ignore it", result.stderr)
        self.assertIn("DEPENDENCIES.md", result.stderr)
        self.assertEqual((self.repo / "DEPENDENCIES.md").read_text(), "hand-written\n")

    def test_a_symlinked_target_is_reported_and_left_whole(self):
        """Neither answer for a symlink is the right one, so it is not removed.

        `rm -rf` on the link removes the link and leaves the bytes, which
        reclaims nothing while reporting a reclaim; following it deletes a tree
        outside every root this script checked.
        """
        elsewhere = self.root / "elsewhere"
        (elsewhere / "debug").mkdir(parents=True)
        (elsewhere / "debug/blob").write_bytes(b"z" * 4096)
        (self.repo / "target").symlink_to(elsewhere)
        result = self.clean("build")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("symlink, skipped", result.stderr)
        self.assertTrue((self.repo / "target").is_symlink())
        self.assertTrue((elsewhere / "debug/blob").exists())

    def test_index_scope_stops_each_daemon_before_removing_its_index(self):
        """The daemon serves the index it is being deprived of, and writes to it.

        Removing .pixel under a live daemon races its next write, which is how
        a half-written index outlives the cleanup that was supposed to end it.
        """
        self.build_output(self.repo)
        self.build_output(self.worktree)
        result = self.clean("index")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.repo / ".pixel").exists())
        self.assertFalse((self.worktree / ".pixel").exists())
        self.assertTrue((self.repo / "target/debug/blob").exists())
        self.assertEqual(
            sorted(self.logged_pixel_calls()),
            sorted([f"daemon stop {self.repo} --metrics off",
                    f"daemon stop {self.worktree} --metrics off"]),
        )
        self.assertIn("pixel build-index --history .", result.stdout)

    def test_cache_scope_spares_the_daemon_sockets(self):
        """The sockets live next to the shards and are not cache entries.

        A shard comes back on the next index build; a socket removed under a
        running daemon takes the daemon's clients with it.
        """
        cache = self.root / "cache/pixel"
        (cache / "shards").mkdir(parents=True)
        (cache / "shards/abc.shard").write_bytes(b"s" * 4096)
        (cache / "sockets").mkdir()
        (cache / "sockets/pixel.sock").write_text("")
        result = self.clean("cache")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((cache / "shards").exists())
        self.assertTrue((cache / "sockets/pixel.sock").exists())

    def test_cargo_target_dir_replaces_the_per_worktree_targets(self):
        """cargo wrote the build there, so that is the directory to reclaim.

        The per-worktree `target` directories are then not cargo's output --
        whatever is in them predates the variable -- and removing them would be
        removing something this script was never told about.
        """
        shared = self.root / "shared-target"
        (shared / "debug").mkdir(parents=True)
        (shared / "debug/blob").write_bytes(b"x" * 4096)
        self.build_output(self.repo)
        result = self.clean("build", CARGO_TARGET_DIR=str(shared))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(shared.exists())
        self.assertTrue((self.repo / "target/debug/blob").exists())

    def test_help_prints_the_whole_header_and_no_code(self):
        """The recipes point at `--help` for what each scope costs to rebuild.

        It is the script's own header, cut at the first line that is not a
        comment. Cut by line number instead, the edit that adds a line to the
        header truncates the documentation mid-sentence and nothing goes red.
        """
        result = self.clean("--help")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Reclaim the disk", result.stdout)
        self.assertIn("/tmp bench prefix", result.stdout.replace("\n", " "))
        self.assertNotIn("set -eu", result.stdout)

    def test_bench_scope_matches_only_the_bench_prefix(self):
        """The bench scratch is identified by where it is and what it is called.

        /tmp holds everyone's scratch, so the prefix is the whole guard: this
        case asserts on the plan rather than on a removal, so that running the
        suite never costs a developer a neighbouring directory.
        """
        token = f"pixel-bench-contract-{os.getpid()}"
        mine = Path("/tmp") / token
        neighbour = Path("/tmp") / f"pixel-other-contract-{os.getpid()}"
        for d in (mine, neighbour):
            d.mkdir(exist_ok=True)
            self.addCleanup(shutil.rmtree, d, ignore_errors=True)
        result = self.clean("bench", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str(mine), result.stdout)
        self.assertNotIn(str(neighbour), result.stdout)
        self.assertTrue(mine.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
