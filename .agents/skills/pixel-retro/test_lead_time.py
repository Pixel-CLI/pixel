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


class Repository(unittest.TestCase):
    """The 2026-10-16 retro measured pixel-mvp's #33 and #36 as this
    repository's #33 and #36 (the project-directory glob took the sibling
    `pixel-mvp` for a worktree) and read their month-old checks: a lead time
    of -48 269 min. A PR is this repository's by its URL, nothing else."""

    def session(self, url, command="gh pr create --fill"):
        return lt.load_events([
            assistant(1, ("e1", "Edit", {})),
            result(2, "e1"),
            assistant(5, ("p1", "Bash", bash(command))),
            result(6, "p1", url),
        ])

    def test_a_pr_of_another_repository_with_the_same_number_is_not_this_ones(self):
        events = self.session("https://github.com/Pixel-CLI/pixel-mvp/pull/33")
        self.assertEqual(lt.units(events, "Pixel-CLI/pixel"), [])
        self.assertEqual([u["pr"] for u in lt.units(self.session("https://github.com/pixel-cli/PIXEL/pull/33"),
                                                    "Pixel-CLI/pixel")], [33])

    def test_gh_pr_create_quoted_inside_another_command_opens_nothing(self):
        # The retro's own `grep 'gh pr create'` over a rollout printed PR URLs.
        events = self.session("https://github.com/Pixel-CLI/pixel/pull/1012", "grep -o 'gh pr create' rollout.jsonl")
        self.assertEqual(lt.units(events, "Pixel-CLI/pixel"), [])
        for command in ("cd /x && gh pr create --fill", "git push -u origin b\ngh pr create --base main",
                        "GH_PAGER= gh pr create --fill"):
            events = self.session("https://github.com/Pixel-CLI/pixel/pull/7", command)
            self.assertEqual([u["pr"] for u in lt.units(events, "Pixel-CLI/pixel")], [7], command)

    def test_a_subagents_prompt_is_not_the_human(self):
        lines = [user(0, "open the PR for #12"), assistant(1, text="on it")]
        self.assertEqual([lt.next_kind(e) for e in lt.load_events(lines)], ["human", "model"])
        self.assertEqual([lt.next_kind(e) for e in lt.load_events(lines, subagent=True)], ["idle", "model"])


def codex(minutes, kind, **payload):
    return json.dumps({"timestamp": at(minutes), "type": "response_item", "payload": {"type": kind, **payload}})


def codex_exec(minutes, call_id, *calls):
    script = "\n".join(f"text(await tools.{call});" for call in calls)
    return codex(minutes, "custom_tool_call", name="exec", call_id=call_id, input=script)


def codex_output(minutes, call_id, text):
    return codex(minutes, "custom_tool_call_output", call_id=call_id, output=[{"type": "input_text", "text": text}])


class Codex(unittest.TestCase):
    """Every PR of the 2026-10-16 window came from Codex, and lead_time.py
    measured none: it read Claude Code transcripts only."""

    def rollout(self):
        return [
            json.dumps({"timestamp": at(0), "type": "session_meta", "payload": {"id": "01a14247-x", "cwd": "/r"}}),
            codex(0, "message", role="user", content=[{"type": "input_text", "text": "# AGENTS.md instructions for /r"}]),
            codex(0, "message", role="user", content=[{"type": "input_text", "text": "<heartbeat>tick</heartbeat>"}]),
            codex(1, "message", role="user", content=[{"type": "input_text", "text": "fix #1011"}]),
            codex_exec(2, "c1", 'exec_command({cmd:"rg -n write_settings crates"})'),
            codex_output(3, "c1", "crates/a.rs:1"),
            # Codex edits from a heredoc script as often as with apply_patch.
            codex_exec(4, "c2", 'exec_command({cmd:"python3 - <<\'PY\'\\np=Path(\'a.rs\');p.write_text(s)\\nPY"})'),
            codex_output(5, "c2", "ok"),
            # gh pr create still runs when the call returns: the URL comes back
            # through the poll of the same session.
            codex_exec(10, "c3", 'exec_command({cmd:"git push -u origin b\\ngh pr create --base main --body-file /tmp/b.md",yield_time_ms:1000})'),
            codex_output(11, "c3", '{"session_id":16372,"output":"[b 79f657ca] fix"}'),
            codex(12, "function_call", name="wait", call_id="w1", arguments='{"cell_id":"3"}'),
            codex(20, "function_call_output", call_id="w1", output="Wall time: 8 min"),
            codex_exec(21, "c4", 'write_stdin({session_id:16372,chars:"",yield_time_ms:1000})'),
            codex_output(22, "c4", '{"output":"https://github.com/Pixel-CLI/pixel/pull/1012\\n"}'),
        ]

    def test_a_codex_pr_starts_at_its_first_shell_edit_and_ends_at_the_poll_that_names_it(self):
        session, cwd, events = lt.load_codex(self.rollout())
        self.assertEqual((session, cwd), ("01a14247-x", "/r"))
        found = lt.units(events, "Pixel-CLI/pixel")
        self.assertEqual([(u["pr"], u["start"], u["opened"]) for u in found],
                         [(1012, T0 + datetime.timedelta(minutes=4), T0 + datetime.timedelta(minutes=10))])

    def test_codex_context_is_not_the_human_and_a_wait_is_idle(self):
        _, _, events = lt.load_codex(self.rollout())
        buckets, blocking, pushes = lt.breakdown(events, T0, T0 + datetime.timedelta(minutes=22))
        # Only "fix #1011" (minute 0 → 1) is a human gap; the injected
        # AGENTS.md and heartbeat messages are not.
        self.assertEqual(buckets["human"], 60.0)
        # The wait (minute 12 → 20) and the poll that only reads a running
        # command (21 → 22) are idle, charged to the wait.
        self.assertEqual(buckets["idle"], 9 * 60.0)
        self.assertEqual(blocking["wait (background)"], 9 * 60.0)
        self.assertEqual(pushes, 1)


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
            ("S=/tmp/a && sh scripts/ci/gates.sh --force", "sh gates.sh"),
            ("bash -c 'cargo test -p pixel-cli'", "cargo test"),
            ("timeout 600 python3 lead_time.py 3d", "python3 lead_time.py"),
            # Three words at most: every PR's watch lands in one group.
            ("RUST_LOG=1 gh pr checks 7 --watch | tail", "gh pr checks"),
        ]:
            self.assertEqual(lt.bash_key(command), key, command)


if __name__ == "__main__":
    unittest.main()
