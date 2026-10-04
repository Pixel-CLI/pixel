#!/usr/bin/env python3
"""Contract of scripts/cancel-stale-runs.sh, the sweep cancel-stale.yml runs
when a pull request closes and every ten minutes.

It exists to stop validation runs whose verdict no longer matters. What it
must leave alone matters as much: the board sync (a pull_request_target run
the merge itself triggers) records the merge on the project board, and a
sweep that cancels it leaves merged tasks In Progress; the sweep's own run,
cancelled mid-loop, leaves the rest of the queue standing.

`gh` is a stub on PATH: it answers the runs and open-PR listings from files
and logs every cancel call, so the test asserts which ids were cancelled.
"""
from __future__ import annotations

import datetime
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "cancel-stale-runs.sh"

STUB_GH = r"""#!/usr/bin/env bash
# Answers `gh api` the way cancel-stale-runs.sh reads it (its --jq output).
url=""
method=GET
for arg in "$@"; do
    case "$arg" in
        repos/*) url=$arg ;;
        POST) method=POST ;;
    esac
done
if [ "$method" = POST ]; then
    printf '%s\n' "$url" >> "$GH_LOG"
    exit 0
fi
case "$url" in
    *"status=queued"*"page=1") cat "$STUB_DIR/queued.tsv" 2>/dev/null ;;
    *"status=in_progress"*"page=1") cat "$STUB_DIR/in_progress.tsv" 2>/dev/null ;;
    *"pulls?state=open"*) cat "$STUB_DIR/open_branches.txt" 2>/dev/null ;;
esac
"""


def now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class CancelStaleRuns(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.bin = root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(STUB_GH)
        gh.chmod(0o755)
        self.stub = root / "stub"
        self.stub.mkdir()
        self.log = root / "gh.log"

    def tearDown(self):
        self.tmp.cleanup()

    def runs(self, rows: list[tuple[int, str, str]], status: str = "in_progress") -> None:
        lines = [f"{rid}\t{head}\t{event}\t{now()}" for rid, head, event in rows]
        (self.stub / f"{status}.tsv").write_text("\n".join(lines) + "\n")

    def sweep(self, *args: str, run_id: str = "") -> subprocess.CompletedProcess:
        env = dict(os.environ)
        env.update(
            PATH=f"{self.bin}:{env['PATH']}",
            STUB_DIR=str(self.stub),
            GH_LOG=str(self.log),
            GITHUB_RUN_ID=run_id,
        )
        return subprocess.run(
            ["bash", str(SCRIPT), "--repo", "o/r", *args],
            env=env, capture_output=True, text=True, check=True,
        )

    def cancelled(self) -> set[int]:
        if not self.log.exists():
            return set()
        return {int(line.split("/")[5]) for line in self.log.read_text().split() if line}

    def test_merge_cancels_validation_but_not_the_board_sync_or_itself(self):
        self.runs([
            (1, "feat", "pull_request"),         # CI on the merged head: moot
            (2, "feat", "pull_request_target"),  # board sync of the merge
            (3, "feat", "pull_request"),         # this sweep's own run
            (4, "other", "pull_request"),        # another branch's CI
        ])
        self.sweep("--branch", "feat", "--apply", run_id="3")
        self.assertEqual(self.cancelled(), {1})

    def test_periodic_sweep_spares_board_sync_and_open_branches(self):
        (self.stub / "open_branches.txt").write_text("live\n")
        self.runs([
            (10, "gone", "pull_request"),         # closed PR's CI: cancelled
            (11, "gone", "pull_request_target"),  # its board sync: kept
            (12, "live", "pull_request"),         # open PR: kept
            (13, "main", "push"),                 # main: kept
        ])
        self.sweep("--closed", "--apply")
        self.assertEqual(self.cancelled(), {10})

    def test_dry_run_cancels_nothing(self):
        self.runs([(20, "feat", "pull_request")])
        out = self.sweep("--branch", "feat")
        self.assertEqual(self.cancelled(), set())
        self.assertIn("would cancel 20", out.stdout)


if __name__ == "__main__":
    unittest.main()
