#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/mutants-nightly.py: the rotation and the tracking issue.

What must hold: the seven nights together cover every shard of the
whole-tree list exactly once, the count a night expects is the count its
shards receive (otherwise a crashed shard passes as a complete slice), a
survivor or an unjudged mutant makes the night red, and rewriting one
night's section of the issue leaves the other nights' sections intact.
"""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("mutants-nightly.py")
WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/mutants-nightly.yml"
_spec = importlib.util.spec_from_file_location("mutants_nightly", SCRIPT)
nightly = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(nightly)


def outcomes(root: Path, shard: str, summaries: list[tuple[str, str]]) -> None:
    """One shard's mutants.out, shaped like cargo-mutants 27.1 writes it."""
    out = root / shard
    out.mkdir(parents=True)
    scenarios = [{"scenario": "Baseline", "summary": "Success"}]
    scenarios += [{"scenario": {"Mutant": {"name": name}}, "summary": summary}
                  for name, summary in summaries]
    (out / "outcomes.json").write_text(json.dumps({"outcomes": scenarios}))


class Rotation(unittest.TestCase):
    def test_the_week_covers_every_shard_exactly_once(self):
        shards = [s for d in range(nightly.SLICES) for s in nightly.slice_shards(d)]
        self.assertEqual(len(shards), len(set(shards)))
        self.assertEqual(sorted(int(s.split("/")[0]) for s in shards), list(range(nightly.TOTAL_SHARDS)))
        self.assertTrue(all(s.endswith(f"/{nightly.TOTAL_SHARDS}") for s in shards))

    def test_the_week_expects_every_mutant_exactly_once(self):
        for total in (0, 1, 69, 70, 71, 13_624):
            with self.subTest(total=total):
                self.assertEqual(sum(nightly.expected_mutants(total, d) for d in range(nightly.SLICES)), total)

    def test_a_night_expects_what_round_robin_gives_its_shards(self):
        # 13 624 = 194 * 70 + 44: shards 0..43 get 195 mutants, 44..69 get 194.
        self.assertEqual(nightly.expected_mutants(13_624, 0), 1950)
        self.assertEqual(nightly.expected_mutants(13_624, 4), 4 * 195 + 6 * 194)
        self.assertEqual(nightly.expected_mutants(13_624, 6), 1940)

    def test_a_slice_outside_the_week_is_refused(self):
        with self.assertRaises(ValueError):
            nightly.slice_shards(nightly.SLICES)

    def test_plan_maps_the_weekday_and_writes_the_matrix(self):
        with tempfile.TemporaryDirectory() as tmp:
            listing = Path(tmp, "list.txt")
            listing.write_text("".join(f"crates/a/src/lib.rs:{i}:1: replace f -> u8 with 0\n" for i in range(140)))
            out = Path(tmp, "out")
            self.assertEqual(nightly.main(["plan", "--weekday", "2", "--list", str(listing), "--github-output", str(out)]), 0)
            written = dict(line.split("=", 1) for line in out.read_text().splitlines())
        self.assertEqual(written["slice"], "2")
        self.assertEqual(json.loads(written["shards"]), [f"{k}/70" for k in range(20, 30)])
        self.assertEqual(written["expected"], "20")
        self.assertEqual(written["total"], "140")


class Report(unittest.TestCase):
    def run_report(self, root: Path, expected: int, body: str = "") -> tuple[int, str]:
        issue = root.parent / "issue.md"
        issue.write_text(body)
        out = root.parent / "new.md"
        import contextlib, io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = nightly.main(["report", "--slice", "3", "--expected", str(expected),
                                 "--outcomes-root", str(root), "--run-url", "https://example/run/1",
                                 "--sha", "0123456789abcdef", "--issue-body", str(issue)])
        out.write_text(buf.getvalue())
        return code, buf.getvalue()

    def test_a_held_slice_is_green_and_says_so(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant"), ("g", "Unviable")])
            code, body = self.run_report(root, 2)
        self.assertEqual(code, 0)
        self.assertIn("### Slice 3 (held)", body)
        self.assertIn("2 mutant(s) assigned, 2 judged (1 caught, 1 unviable)", body)

    def test_a_survivor_makes_the_night_red_and_is_named(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            outcomes(root, "b", [("crates/x.rs:3:1: replace g -> bool with true", "MissedMutant")])
            code, body = self.run_report(root, 2)
        self.assertEqual(code, 1)
        self.assertIn("MISSED crates/x.rs:3:1: replace g -> bool with true", body)
        self.assertIn("(needs work)", body)

    def test_a_shard_that_never_reported_leaves_the_night_red(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            code, body = self.run_report(root, 195)
        self.assertEqual(code, 1)
        self.assertIn("195 mutant(s) listed, 1 reached a verdict", body)

    def test_no_artifact_at_all_is_a_red_night_not_a_crash(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, body = self.run_report(Path(tmp, "missing"), 10)
        self.assertEqual(code, 1)
        self.assertIn("0 judged (no outcome)", body)

    def test_a_long_survivor_list_stays_within_the_section_budget(self):
        names = [f"crates/pixel/src/some/long/module_{i:04}.rs:{i}:9: replace a_function -> bool with true"
                 for i in range(2_000)]
        sec = nightly.section(4, 2_000, nightly.Counter(missed=2_000), [f"MISSED {n}" for n in names],
                              "2000 mutant(s) survived", "https://example/run/1", "a" * 40)
        self.assertLessEqual(len(sec), nightly.SECTION_BUDGET)
        named = sec.count("MISSED crates/")
        self.assertGreater(named, 0)
        self.assertIn(f"{2_000 - named} more in the run's", sec)
        self.assertTrue(sec.endswith(nightly.end(4)))

    def test_seven_full_nights_fit_github_s_issue_limit(self):
        survivors = [f"MISSED crates/x/src/m.rs:{i}:9: replace f_{i} -> Option<String> with None" for i in range(5_000)]
        body = ""
        for d in range(nightly.SLICES):
            body = nightly.update_body(body, d, nightly.section(
                d, 5_000, nightly.Counter(missed=5_000), survivors, "5000 mutant(s) survived",
                "https://github.com/Pixel-CLI/pixel/actions/runs/99999999999", "b" * 40))
        self.assertLessEqual(len(body) + nightly.PROSE_ALLOWANCE - len(nightly.HEADER), nightly.ISSUE_BODY_LIMIT)

    def test_prose_too_long_for_the_issue_fails_before_writing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            import contextlib, io
            issue = Path(tmp, "issue.md")
            issue.write_text(nightly.HEADER + "\n\n" + "x" * nightly.ISSUE_BODY_LIMIT)
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = nightly.main(["report", "--slice", "0", "--expected", "1", "--outcomes-root", str(root),
                                     "--run-url", "u", "--sha", "s" * 12, "--issue-body", str(issue)])
        self.assertEqual(code, 2)
        self.assertEqual(out.getvalue(), "")
        self.assertIn("over GitHub's 65536", err.getvalue())

    def test_rewriting_one_night_keeps_the_others(self):
        other = nightly.section(1, 5, nightly.Counter(caught=5), [], None, "https://example/run/0", "f" * 40)
        stale = nightly.section(3, 9, nightly.Counter(missed=9), ["MISSED old"], "stale", "https://example/run/-1", "e" * 40)
        body = nightly.update_body(nightly.update_body("", 1, other), 3, stale)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            _, new = self.run_report(root, 1, body)
        self.assertIn(other, new)
        self.assertNotIn("MISSED old", new)
        self.assertEqual(new.count("<!-- mutants-nightly slice 3 -->"), 1)
        self.assertLess(new.index("slice 1 -->"), new.index("slice 3 -->"))
        self.assertEqual(new.count(nightly.HEADER), 1)

    def test_notes_outside_the_sections_survive_every_rewrite(self):
        note = "Triage: slice 2's survivors in pixel-rank are tracked in #999."
        body = nightly.update_body("", 4, nightly.section(4, 0, nightly.Counter(), [], None, "u", "s" * 12))
        body = body.replace(nightly.HEADER, nightly.HEADER + "\n\n" + note)
        body += "\nFooter note kept below the sections.\n"
        for d in (1, 6, 4):
            body = nightly.update_body(body, d, nightly.section(d, 0, nightly.Counter(), [], None, "u", "s" * 12))
        self.assertIn(note, body)
        self.assertIn("Footer note kept below the sections.", body)
        order = [int(x) for x in __import__("re").findall(r"<!-- mutants-nightly slice (\d+) -->", body)]
        self.assertEqual(order, [1, 4, 6])

    def test_a_new_night_is_inserted_in_slice_order(self):
        body = ""
        for d in (5, 0, 3):
            body = nightly.update_body(body, d, nightly.section(d, 0, nightly.Counter(), [], None, "u", "s" * 12))
        order = [int(x) for x in __import__("re").findall(r"<!-- mutants-nightly slice (\d+) -->", body)]
        self.assertEqual(order, [0, 3, 5])


def issue_step_shell() -> str:
    """The workflow's `Update the tracking issue` step, dedented as a script."""
    block = WORKFLOW.read_text().split("- name: Update the tracking issue", 1)[1]
    body = block.split("run: |", 1)[1].splitlines()[1:]
    lines = []
    for line in body:
        if line.strip() and not line.startswith("          "):
            break
        lines.append(line[10:])
    return "\n".join(lines)


class TrackingIssueWrite(unittest.TestCase):
    """The report step must never replace the issue with an empty body.

    `> new.md` truncates the file before the reporter runs, and the reporter
    prints nothing when it fails before the end (prose over the limit exits
    2). `gh issue edit --body-file new.md` on that empty file would erase the
    other six slices and any human note; the write is guarded on a non-empty
    body and the step still fails with the reporter's exit code.
    """

    def stubs(self, tmp: Path, reporter: str, listed: str) -> Path:
        """`python3` and `gh` in front of PATH; the latter records every call."""
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        calls = tmp / "gh-calls"
        (bin_dir / "python3").write_text(f"#!/bin/sh\n{reporter}\n")
        (bin_dir / "gh").write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$*\" >> '{calls}'\n"
            "case \"$1 $2\" in\n"
            f"  'issue list') printf '%s' '{listed}' ;;\n"
            "  'issue view') printf 'the existing body\\n' ;;\n"
            "esac\n"
        )
        for stub in (bin_dir / "python3", bin_dir / "gh"):
            stub.chmod(0o755)
        return calls

    def run_step(self, tmp: Path, reporter: str, listed: str) -> tuple[subprocess.CompletedProcess[str], Path]:
        """The step's own shell, run by bash -e as the runner runs it."""
        calls = self.stubs(tmp, reporter, listed)
        env = {
            **os.environ,
            "PATH": f"{tmp / 'bin'}:{os.environ['PATH']}",
            "GH_CALLS": str(calls),
            "GH_TOKEN": "x",
            "SLICE": "3",
            "EXPECTED": "2",
            "RUN_URL": "https://example/run/1",
            "GITHUB_SHA": "a" * 40,
            "GITHUB_STEP_SUMMARY": str(tmp / "summary.md"),
        }
        result = subprocess.run(["bash", "-e", "-c", issue_step_shell()], cwd=tmp, env=env,
                                capture_output=True, text=True)
        return result, calls

    def test_a_failed_reporter_leaves_an_existing_issue_untouched(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, calls = self.run_step(Path(tmp), "exit 2", "7")
            self.assertEqual(result.returncode, 2, result.stderr)
            writes = [c for c in calls.read_text().splitlines()
                      if c.startswith(("issue edit", "issue create"))]
            self.assertEqual(writes, [], result.stderr)
            self.assertIn("leaving the tracking issue untouched", result.stderr)

    def test_a_failed_reporter_creates_no_issue(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, calls = self.run_step(Path(tmp), "exit 2", "")
            self.assertEqual(result.returncode, 2, result.stderr)
            writes = [c for c in calls.read_text().splitlines()
                      if c.startswith(("issue edit", "issue create"))]
            self.assertEqual(writes, [], result.stderr)

    def test_a_reported_body_reaches_the_issue_and_keeps_the_exit_code(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, calls = self.run_step(Path(tmp), "printf 'NEW BODY\\n'\nexit 1", "7")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("issue edit 7 --body-file new.md", calls.read_text())
            self.assertEqual((Path(tmp) / "new.md").read_text(), "NEW BODY\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
