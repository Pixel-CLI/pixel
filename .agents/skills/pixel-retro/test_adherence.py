#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of adherence.py: which call counts as what, and which numbers come out.

Its numbers decide whether a hook, prompt or routing change moved agent
behaviour, so each case pins a way to get them wrong: a `| grep` filter on
Pixel output counted as a native search, a bounded `sed -n` counted as an
unbounded read, a Codex edit inside an `exec` script missed, hosts pooled,
a session outside an indexed repository counted.
"""

import datetime
import json
import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import adherence as ad  # noqa: E402

T0 = datetime.datetime(2026, 10, 1, 10, 0, tzinfo=datetime.timezone.utc)


def at(minutes):
    return (T0 + datetime.timedelta(minutes=minutes)).isoformat().replace("+00:00", "Z")


def claude_use(minutes, name, inp, cwd):
    return json.dumps({"type": "assistant", "timestamp": at(minutes), "cwd": cwd,
                       "message": {"content": [{"type": "tool_use", "id": f"t{minutes}", "name": name,
                                                "input": inp}]}})


def codex_exec(minutes, script, cwd):
    return json.dumps({"timestamp": at(minutes), "type": "response_item",
                       "payload": {"type": "custom_tool_call", "name": "exec", "input": script, "cwd": cwd}})


class ShellClassification(unittest.TestCase):
    def test_pixel_retrieval_is_pixel_and_its_filter_is_not_a_search(self):
        self.assertEqual(ad.shell_events("pixel search-content -F foo | grep bar"), [("pixel", "search-content")])
        self.assertEqual(ad.shell_events("rtk pixel find-code 'x' 2>&1 | head -40"), [("pixel", "find-code")])
        self.assertEqual(ad.shell_events("pixel status"), [], "status is not retrieval")

    def test_quoted_text_and_heredoc_bodies_are_not_commands(self):
        self.assertEqual(ad.shell_events('echo "noise; pixel impact X; rg foo"'), [])
        self.assertEqual(ad.shell_events("echo 'a | grep b' && rg c"), [("search", "rg")])
        heredoc = "cat <<'EOF' > notes.md\npixel impact Foo\nrg needle\nEOF\nrg after"
        self.assertEqual(ad.shell_events(heredoc), [("search", "rg")])

    def test_native_searches_in_any_statement(self):
        self.assertEqual(ad.shell_events("cd /repo && rg -n foo src; git grep bar"),
                         [("search", "rg"), ("search", "git")])
        self.assertEqual(ad.shell_events("find . -name '*.rs'"), [("search", "find")])
        self.assertEqual(ad.shell_events("find . -type f -newer x"), [], "a find without a name test lists")

    def test_read_widths(self):
        self.assertEqual(ad.shell_events("sed -n '10,49p' a.rs"), [("read", 40)])
        self.assertEqual(ad.shell_events("sed -n '10,+40p' a.rs"), [("read", 41)])
        self.assertEqual(ad.shell_events("sed -n 12p a.rs"), [("read", 1)])
        self.assertEqual(ad.shell_events("sed -n '5,$p' a.rs"), [("read", None)])
        self.assertEqual(ad.shell_events("sed -i s/a/b/ a.rs"), [], "an in-place edit is not a read")
        self.assertEqual(ad.shell_events("cat a.rs"), [("read", None)])
        self.assertEqual(ad.shell_events("head -n 30 a.rs"), [("read", 30)])
        self.assertEqual(ad.shell_events("tail a.rs"), [("read", 10)])
        self.assertEqual(ad.shell_events("head -n -40 a.rs"), [("read", None)], "all but the last 40")
        self.assertEqual(ad.shell_events("tail -n +40 a.rs"), [("read", None)], "from line 40 on")
        self.assertEqual(ad.shell_events("tail -n 40 a.rs"), [("read", 40)])
        self.assertEqual(ad.shell_events("head -25 a.rs"), [("read", 25)])
        self.assertEqual(ad.shell_events("cat"), [], "no operand reads stdin")

    def test_claude_tools(self):
        self.assertEqual(ad.claude_events("Read", {"file_path": "a", "limit": 40}), [("read", 40)])
        self.assertEqual(ad.claude_events("Read", {"file_path": "a", "offset": 10}), [("read", None)])
        self.assertEqual(ad.claude_events("Grep", {}), [("search", "Grep")])
        self.assertEqual(ad.claude_events("MultiEdit", {}), [("edit", None)])
        self.assertEqual(ad.claude_events("mcp__pixel__find_code", {}), [("pixel", "find-code")])
        self.assertEqual(ad.claude_events("mcp__pixel__who_calls", {}), [("pixel", "who-calls")])
        self.assertEqual(ad.claude_events("mcp__pixel__status", {}), [], "status is not retrieval")

    def test_codex_exec_scripts_keep_call_order(self):
        script = ('const a = await tools.exec_command({"cmd":"rg -n foo src","workdir":"/r"});\n'
                  'await tools.apply_patch("*** Begin Patch");\n'
                  'const b = await tools.exec_command({"cmd":"pixel impact \\"Foo::bar\\"","workdir":"/r"});')
        self.assertEqual(ad.exec_script_events(script),
                         [("search", "rg"), ("edit", None), ("pixel", "impact")])

    def test_each_codex_call_reads_its_own_command(self):
        # An unquoted key on the first call must not borrow the next call's command.
        script = ('await tools.exec_command({cmd: "rg -n foo src"});\n'
                  'await tools.apply_patch("x");\n'
                  'await tools.exec_command({"cmd": "pixel impact Bar"});')
        self.assertEqual(ad.exec_script_events(script),
                         [("search", "rg"), ("edit", None), ("pixel", "impact")])


class Summary(unittest.TestCase):
    def test_counts_per_session_stream(self):
        sessions = [
            # Pixel first, then a native search: the answer was not trusted.
            [("pixel", "search-content"), ("search", "rg"), ("read", None), ("edit", None)],
            # Native first; a structural Pixel call before the first edit.
            [("search", "Grep"), ("pixel", "impact"), ("read", 40), ("edit", None)],
            # No search at all.
            [("read", 200)],
        ]
        s = ad.summarize(sessions)
        self.assertEqual(s["sessions"], 3)
        self.assertEqual((s["pixel_calls"], s["native_searches"]), (2, 2))
        self.assertEqual(s["pixel_share"], 0.5)
        self.assertEqual((s["first_search_pixel"], s["sessions_with_search"]), (1, 2))
        self.assertEqual(s["sessions_with_pixel"], 2)
        self.assertEqual(s["pixel_then_native"], 1, "of 2 Pixel calls")
        self.assertEqual((s["reads"], s["unbounded_reads"], s["wide_reads"]), (3, 1, 1))
        # The read after `search-content` follows a native search, so it is not
        # a read after Pixel; the 40-line read after `impact` is.
        self.assertEqual((s["reads_after_pixel"], s["unbounded_after_pixel"]), (1, 0))
        self.assertEqual(s["median_read_width_after_pixel"], 40)
        self.assertEqual((s["impact_before_edit"], s["edit_sessions"]), (1, 2))

    def test_after_pixel_means_the_very_next_event(self):
        s = ad.summarize([[("pixel", "find-code"), ("edit", None), ("search", "rg"), ("read", None)]])
        self.assertEqual(s["pixel_then_native"], 0, "an edit came in between")
        self.assertEqual(s["reads_after_pixel"], 0)
        s = ad.summarize([[("pixel", "find-code"), ("read", 30), ("pixel", "impact"), ("search", "Grep")]])
        self.assertEqual((s["reads_after_pixel"], s["pixel_then_native"]), (1, 1))

    def test_empty_host_reports_no_share(self):
        s = ad.summarize([])
        self.assertIsNone(s["pixel_share"])
        self.assertIsNone(s["median_read_width_after_pixel"])


class Collect(unittest.TestCase):
    def test_hosts_are_separate_and_unindexed_repositories_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            indexed, plain = tmp / "indexed", tmp / "plain"
            for repo in (indexed, plain):
                (repo / ".git").mkdir(parents=True)
            (indexed / ".pixel").mkdir()
            projects = tmp / "claude"
            (projects / "p").mkdir(parents=True)
            (projects / "p" / "s1.jsonl").write_text("\n".join([
                claude_use(1, "Bash", {"command": "pixel search-content -F foo"}, str(indexed)),
                claude_use(2, "Grep", {"pattern": "foo"}, str(indexed)),
            ]))
            (projects / "p" / "s2.jsonl").write_text(claude_use(1, "Grep", {"pattern": "x"}, str(plain)))
            # Active in the window, in the indexed repository, with no retrieval at all.
            (projects / "p" / "s3.jsonl").write_text(claude_use(3, "TodoWrite", {}, str(indexed)))
            codex = tmp / "codex" / "2026" / "10" / "01"
            codex.mkdir(parents=True)
            (codex / "rollout-a.jsonl").write_text("\n".join([
                json.dumps({"timestamp": at(0), "type": "session_meta", "payload": {"cwd": str(indexed)}}),
                codex_exec(1, 'await tools.exec_command({"cmd":"rg foo"})', str(indexed)),
            ]))
            start = T0 - datetime.timedelta(hours=1)
            result = ad.collect(start, [projects], tmp / "codex", all_repos=False)
            self.assertEqual(result["claude"]["sessions"], 2,
                             "the plain repository is not counted; an active session without retrieval is")
            self.assertEqual(result["claude"]["sessions_with_pixel"], 1)
            self.assertEqual((result["claude"]["pixel_calls"], result["claude"]["native_searches"]), (1, 1))
            self.assertEqual((result["codex"]["pixel_calls"], result["codex"]["native_searches"]), (0, 1))
            self.assertEqual(ad.collect(start, [projects], tmp / "codex", all_repos=True)["claude"]["sessions"], 3)
            later = T0 + datetime.timedelta(minutes=5)
            self.assertEqual(ad.collect(later, [projects], tmp / "codex", all_repos=True)["claude"]["sessions"], 0,
                             "events before the window start are not counted")


if __name__ == "__main__":
    unittest.main(verbosity=1)
