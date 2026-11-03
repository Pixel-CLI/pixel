#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of slow.py: each slow invocation counted once, in the repository it ran in.

The 2026-10-16 retro read one 7.3 s `find-symbol` as four, in four
repositories: a worktree bootstrap had copied the main checkout's
`actions.jsonl` into three Codex worktrees. A slow op that looks four
times as frequent and widespread outranks every real finding.
"""

import json
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import slow  # noqa: E402

TS = 1_792_000_000_000


def line(invocation, cwd, ms=7349, command="find-symbol", **extra):
    return json.dumps({"invocation_id": invocation, "ts_ms": TS, "pid": 1, "command": command,
                       "args": f"{command} x", "cwd": cwd, "duration_ms": ms,
                       "serve": [{"route": "daemon_started", "start_ms": ms}], **extra})


class Copies(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def logs(self, *contents):
        paths = []
        for i, lines in enumerate(contents):
            path = pathlib.Path(self.dir.name) / f"{i}.jsonl"
            path.write_text("\n".join(lines) + "\n")
            paths.append(str(path))
        return paths

    def test_a_line_copied_into_other_logs_is_one_invocation_in_its_own_repository(self):
        main = line("32553-a-0", "/repo")
        paths = self.logs([main], [main, line("2-b-0", "/wt/1")], [main])
        groups, skipped, copies = slow.collect(paths, 0, 5000)
        rows = groups[("find-symbol", "daemon_started", "-", "start_ms")]
        self.assertEqual(sorted(r[1] for r in rows), ["2-b-0", "32553-a-0"])
        # Repositories are where the calls ran, not the files holding them.
        self.assertEqual({r[2] for r in rows}, {"/repo", "/wt/1"})
        self.assertEqual((skipped, copies), (0, 2))

    def test_lines_without_an_id_are_told_apart_by_pid_time_and_args(self):
        bare = json.loads(line(None, "/repo"))
        other = dict(bare, args="find-symbol y")
        paths = self.logs([json.dumps(bare), json.dumps(other)], [json.dumps(bare)])
        groups, _, copies = slow.collect(paths, 0, 5000)
        self.assertEqual(sum(map(len, groups.values())), 2)
        self.assertEqual(copies, 1)


if __name__ == "__main__":
    unittest.main()
