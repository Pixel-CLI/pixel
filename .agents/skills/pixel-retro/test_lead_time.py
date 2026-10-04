#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of lead_time.py: which time each pull request owns, and what it was spent on.

The numbers it prints decide which workflow rule gets changed, so each case
pins a way to get them wrong: a PR charged with the previous PR's edits, a
harness notification counted as the human, an open PR reported as validated
while a check still runs, idle time charged to the wrong command.
"""

import datetime
import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import lead_time as lt  # noqa: E402

T0 = datetime.datetime(2026, 9, 30, 10, 0, tzinfo=datetime.timezone.utc)


def at(minutes):
    return (T0 + datetime.timedelta(minutes=minutes)).isoformat().replace("+00:00", "Z")


def assistant(minutes, *uses, text=""):
    content = [{"type": "tool_use", "id": uid, "name": name, "input": inp} for uid, name, inp in uses]
    if text:
        content.append({"type": "text", "text": text})
    return json.dumps({"type": "assistant", "timestamp": at(minutes), "message": {"content": content}})


def result(minutes, uid, text="ok"):
    return json.dumps({"type": "user", "timestamp": at(minutes),
                       "message": {"content": [{"type": "tool_result", "tool_use_id": uid, "content": text}]}})


def user(minutes, text):
    return json.dumps({"type": "user", "timestamp": at(minutes), "message": {"content": text}})


def bash(command, background=False):
    return {"command": command, **({"run_in_background": True} if background else {})}


class Units(unittest.TestCase):
    def test_each_pr_owns_the_edits_after_the_previous_one(self):
        events = lt.load_events([
            user(0, "fix the two bugs"),
            assistant(1, ("e1", "Edit", {"file_path": "a.rs"})),
            result(2, "e1"),
            assistant(10, ("p1", "Bash", bash("gh pr create --fill"))),
            result(11, "p1", "https://github.com/o/r/pull/7"),
            assistant(20, ("e2", "Write", {"file_path": "b.rs"})),
            result(21, "e2"),
            assistant(30, ("p2", "Bash", bash("gh pr create --base x"))),
            result(31, "p2", "https://github.com/o/r/pull/8\n"),
        ])
        found = lt.units(events)
        self.assertEqual([u["pr"] for u in found], [7, 8])
        self.assertEqual(found[0]["start"], T0 + datetime.timedelta(minutes=1))
        # The second PR starts at its own first edit, not the session's.
        self.assertEqual(found[1]["start"], T0 + datetime.timedelta(minutes=20))
        self.assertEqual(found[1]["opened"], T0 + datetime.timedelta(minutes=30))

    def test_a_failed_pr_create_is_not_a_unit_and_keeps_its_edits_for_the_retry(self):
        events = lt.load_events([
            assistant(1, ("e1", "Edit", {})),
            result(2, "e1"),
            assistant(5, ("p1", "Bash", bash("gh pr create"))),
            result(6, "p1", "error: a pull request already exists"),
            assistant(9, ("p2", "Bash", bash("gh pr create --head b"))),
            result(10, "p2", "https://github.com/o/r/pull/9"),
        ])
        found = lt.units(events)
        self.assertEqual([(u["pr"], u["start"]) for u in found], [(9, T0 + datetime.timedelta(minutes=1))])


class Gaps(unittest.TestCase):
    def test_injected_user_text_is_never_the_human(self):
        kinds = [lt.next_kind(e) for e in lt.load_events([
            user(0, "<task-notification><task-id>x</task-id></task-notification>"),
            user(1, "<system-reminder>ctx</system-reminder>"),
            user(2, "merge the PR"),
            result(3, "u1"),
            assistant(4, text="done"),
        ])]
        self.assertEqual(kinds, ["idle", "idle", "human", "tool", "model"])

    def test_idle_is_charged_to_the_background_command_and_clipped_to_the_unit(self):
        events = lt.load_events([
            assistant(0, ("b1", "Bash", bash("gh pr checks 7 --watch", background=True))),
            result(1, "b1", "running in background"),
            assistant(2, text="waiting"),
            user(12, "<task-notification>done</task-notification>"),
            assistant(13, ("t1", "Bash", bash("cargo test -p pixel-cli"))),
            result(16, "t1"),
            assistant(17, ("g1", "Bash", bash("git push -u origin b"))),
            result(18, "g1"),
            user(40, "thanks"),
        ])
        buckets, blocking, pushes = lt.breakdown(events, T0, T0 + datetime.timedelta(minutes=30))
        self.assertEqual(blocking["gh pr checks (background)"], 600.0)
        self.assertEqual(blocking["cargo test"], 180.0)
        self.assertEqual(buckets["idle"], 600.0)
        self.assertEqual(buckets["tool"], 60.0 + 180.0 + 60.0)
        self.assertEqual(buckets["model"], 60.0 * 3)
        # The human gap runs to minute 40; only the part inside the unit counts.
        self.assertEqual(buckets["human"], 12 * 60.0)
        self.assertEqual(pushes, 1)


class Validation(unittest.TestCase):
    def check(self, status, completed):
        return {"__typename": "CheckRun", "name": "Test", "status": status, "completedAt": completed}

    def test_an_open_pr_with_a_running_check_is_not_validated(self):
        view = {"mergedAt": None, "statusCheckRollup": [self.check("COMPLETED", at(5)), self.check("IN_PROGRESS", None)]}
        self.assertIsNone(lt.validated_at(view))

    def test_a_merged_pr_ignores_a_review_status_left_pending(self):
        view = {"mergedAt": at(9), "statusCheckRollup": [
            self.check("COMPLETED", at(5)), self.check("COMPLETED", at(8)),
            {"__typename": "StatusContext", "context": "CodeRabbit", "state": "PENDING", "startedAt": at(1)},
        ]}
        self.assertEqual(lt.validated_at(view), T0 + datetime.timedelta(minutes=8))

    def test_a_finished_commit_status_counts_at_its_start_and_no_check_is_no_answer(self):
        view = {"mergedAt": None, "statusCheckRollup": [
            self.check("COMPLETED", at(5)),
            {"__typename": "StatusContext", "context": "CodeRabbit", "state": "SUCCESS", "startedAt": at(7)},
        ]}
        self.assertEqual(lt.validated_at(view), T0 + datetime.timedelta(minutes=7))
        self.assertIsNone(lt.validated_at({"mergedAt": None, "statusCheckRollup": []}))


class Keys(unittest.TestCase):
    def test_commands_group_by_what_actually_ran(self):
        for command, key in [
            ("cd /x && cargo nextest run --workspace", "cargo nextest run"),
            ("S=/tmp/a && sh scripts/gates.sh --force", "sh gates.sh"),
            ("bash -c 'cargo test -p pixel-cli'", "cargo test"),
            ("timeout 600 python3 lead_time.py 3d", "python3 lead_time.py"),
            # Three words at most: every PR's watch lands in one group.
            ("RUST_LOG=1 gh pr checks 7 --watch | tail", "gh pr checks"),
        ]:
            self.assertEqual(lt.bash_key(command), key, command)


if __name__ == "__main__":
    unittest.main()
