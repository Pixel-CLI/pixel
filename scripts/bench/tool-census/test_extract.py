#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Parser and ingest tests of extract.py on small synthetic transcripts.

    python3 -I scripts/bench/tool-census/test_extract.py
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import extract  # noqa: E402


def write_jsonl(path, records):
    with open(path, "w", encoding="utf-8") as handle:
        for record in records:
            handle.write(json.dumps(record) + "\n")


def claude_stream():
    return [
        {"type": "system", "subtype": "init", "model": "claude-sonnet-5-5", "cwd": "/r"},
        {"type": "assistant", "message": {"id": "m1", "model": "claude-sonnet-5-5", "usage": {"input_tokens": 10, "output_tokens": 5},
                                           "content": [{"type": "tool_use", "id": "toolu_1", "name": "Grep", "input": {"pattern": "fn main"}},
                                                       {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {"file_path": "/r/a.rs", "offset": 3, "limit": 9}}]}},
        {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a.rs:1:fn main() {}"},
                                                  {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": True,
                                                   "content": [{"type": "text", "text": "x" * 20000}]}]}},
        {"type": "assistant", "message": {"id": "m2", "content": [{"type": "tool_use", "id": "toolu_3", "name": "Edit", "input": {"file_path": "/r/a.rs"}}]}},
        {"type": "result", "subtype": "success", "total_cost_usd": 0.1},
    ]


def codex_rollout():
    script = ('const a = await tools.exec_command({cmd:"rtk grep -rn \\"fn run\\" crates","workdir":"/r"});\n'
              'const cmd = "rtk git status --short";\n'
              'const b = await tools.exec_command({ cmd, workdir: \'/r\' });\n'
              'text(a.output + b.output);\n')
    return [
        {"timestamp": "2026-10-08T10:00:00.000Z", "type": "session_meta", "payload": {"id": "s", "cwd": "/Users/u/Documents/proj", "originator": "codex-tui", "thread_source": "user", "timestamp": "2026-10-08T10:00:00.000Z"}},
        {"timestamp": "2026-10-08T10:00:01.000Z", "type": "turn_context", "payload": {"model": "gpt-6-luna"}},
        {"timestamp": "2026-10-08T10:00:02.000Z", "type": "event_msg", "payload": {"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "text", "text": "find where the run function is defined and show me its callers"}]}}},
        {"timestamp": "2026-10-08T10:00:03.000Z", "type": "response_item", "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "c1", "input": script}},
        {"timestamp": "2026-10-08T10:00:04.000Z", "type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "c1", "output": [{"type": "input_text", "text": "Script completed\nWall time 0.3 seconds\nOutput:\n"}, {"type": "input_text", "text": "crates/a.rs:3:fn run()"}]}},
        {"timestamp": "2026-10-08T10:00:05.000Z", "type": "response_item", "payload": {"type": "function_call", "name": "exec_command", "call_id": "c2", "arguments": json.dumps({"cmd": "cargo test -p a"})}},
        {"timestamp": "2026-10-08T10:00:06.000Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "c2", "output": "Chunk ID: 1\nWall time: 1.0 seconds\nProcess exited with code 101\nOriginal token count: 4\nOutput:\nFAILED\n"}},
        {"timestamp": "2026-10-08T10:00:07.000Z", "type": "response_item", "payload": {"type": "custom_tool_call", "name": "apply_patch", "call_id": "c3", "input": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"}},
        {"timestamp": "2026-10-08T10:00:08.000Z", "type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "c3", "output": "Done"}},
        {"timestamp": "2026-10-08T10:00:09.000Z", "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 100, "output_tokens": 7}}}},
    ]


def pi_session():
    return [
        {"type": "session", "id": "p1", "timestamp": "2026-10-01T10:00:00.000Z", "cwd": "/Users/u/Documents/proj"},
        {"type": "message", "timestamp": "2026-10-01T10:00:01.000Z", "message": {"role": "user", "content": [{"type": "text", "text": "where is the retry logic for failed uploads implemented"}]}},
        {"type": "message", "timestamp": "2026-10-01T10:00:02.000Z", "message": {"role": "assistant", "model": "m", "usage": {"input": 5, "output": 2, "cacheRead": 3},
                                                                            "content": [{"type": "toolCall", "id": "t1", "name": "search", "arguments": {"query": "retryUpload"}}]}},
        {"type": "message", "timestamp": "2026-10-01T10:00:03.000Z", "message": {"role": "toolResult", "toolCallId": "t1", "isError": False, "content": [{"type": "text", "text": "hit"}]}},
    ]


class Parsers(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def path(self, name, records):
        path = os.path.join(self.tmp.name, name)
        write_jsonl(path, records)
        return path

    def test_claude_stream_json_pairs_results_and_caps_output(self):
        session = extract.parse_claude(self.path("s1.stream.jsonl", claude_stream()), "s1", "census", census=True)
        self.assertEqual([c["tool"] for c in session["calls"]], ["Grep", "Read", "Edit"])
        grep, read, edit = session["calls"]
        self.assertEqual((grep["class"], grep["need"]), ("search", "find-definition"))
        self.assertEqual((read["class"], read["need"]), ("read", "read-region"))
        self.assertEqual((edit["class"], edit["need"]), ("edit", "edit-code"))
        self.assertEqual(grep["output"], "a.rs:1:fn main() {}")
        self.assertEqual(read["output_bytes"], 20000)
        self.assertEqual(len(read["output"]), extract.OUTPUT_CAP)
        self.assertEqual((read["is_error"], grep["is_error"]), (1, 0))
        self.assertIsNone(edit["output"])  # no result yet: a live view row
        self.assertEqual({c["turn"] for c in session["calls"]}, {0})
        self.assertEqual(session["model"], "claude-sonnet-5-5")
        self.assertEqual(json.loads(read["input_json"])["offset"], 3)

    def test_claude_session_organic_prompts_and_sidechain(self):
        records = [
            {"type": "user", "timestamp": "2026-10-01T10:00:00.000Z", "cwd": "/r", "message": {"content": "<command-name>/model</command-name>"}},
            {"type": "user", "timestamp": "2026-10-01T10:00:01.000Z", "message": {"content": "MODE: SUBAGENT READ-WRITE. GOAL: x y z w"}},
            {"type": "user", "timestamp": "2026-10-01T10:00:02.000Z", "message": {"content": "fix the flaky retry test in the upload module please"}},
            {"type": "assistant", "timestamp": "2026-10-01T10:00:03.000Z", "message": {"id": "m", "content": [{"type": "tool_use", "id": "toolu_9", "name": "Bash", "input": {"command": "rtk cargo test"}}]}},
            {"type": "user", "timestamp": "2026-10-01T10:00:04.000Z", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_9", "content": "ok"}]}},
            {"type": "user", "timestamp": "2026-10-01T10:00:05.000Z", "message": {"content": "continue"}},
        ]
        session = extract.parse_claude(self.path("h.jsonl", records), "claude-history:h", "claude-history")
        self.assertEqual([p["organic"] for p in session["prompts"]], [True, False])  # the 2nd is a short reply, not organic
        self.assertEqual(session["calls"][0]["turn"], 0)
        self.assertEqual((session["calls"][0]["class"], session["calls"][0]["need"]), ("test", "run-tests"))
        self.assertEqual(session["flags"], [])
        records.append({"type": "user", "isSidechain": True, "message": {"content": "x"}})
        self.assertEqual(extract.parse_claude(self.path("h2.jsonl", records), "h2", "claude-history")["flags"], ["sidechain"])

    def test_codex_rollout_expands_exec_scripts(self):
        session = extract.parse_codex(self.path("rollout.jsonl", codex_rollout()), "codex-history:s")
        tools = [c["tool"] for c in session["calls"]]
        self.assertEqual(tools, ["exec_command", "exec_command", "exec_command", "apply_patch"])
        first, second, test, patch = session["calls"]
        self.assertEqual((first["class"], first["need"]), ("search", "find-definition"))
        self.assertEqual((second["class"], second["need"]), ("git-read", "git-status"))  # `{ cmd }` resolved from `const cmd = ...`
        self.assertEqual(first["output"], "crates/a.rs:3:fn run()")  # the Script header is stripped, the result sits on the first call
        self.assertEqual((second["output"], second["output_bytes"]), ("", 0))
        self.assertEqual((test["class"], test["need"], test["is_error"]), ("test", "run-tests", 1))
        self.assertEqual(test["output"], "FAILED\n")
        self.assertEqual((patch["class"], patch["need"]), ("edit", "edit-code"))
        self.assertEqual([c["turn"] for c in session["calls"]], [0, 0, 0, 0])
        self.assertEqual((session["model"], session["tokens_in"], session["tokens_out"]), ("gpt-6-luna", 100, 7))
        self.assertEqual(session["cwd"], "/Users/u/Documents/proj")
        self.assertTrue(session["prompts"][0]["organic"])

    def test_codex_subagent_threads_are_flagged(self):
        records = codex_rollout()
        records[0]["payload"]["thread_source"] = "subagent"
        self.assertEqual(extract.parse_codex(self.path("sub.jsonl", records), "x")["flags"], ["subagent"])

    def test_js_object_parser(self):
        calls = extract.exec_calls('await tools.exec_command({cmd:"a \\"b\\" c", workdir:\'/w\', yield_time_ms:1000, ok:true});')
        self.assertEqual(calls, [("exec_command", {"cmd": 'a "b" c', "workdir": "/w", "yield_time_ms": 1000, "ok": True})])
        self.assertEqual(extract.exec_calls("text(1)"), [])

    def test_js_expressions_and_command_arrays(self):
        expr = extract.exec_calls('await tools.exec_command({cmd:"rtk bash -c "+q(`cd /x\nrtk rg -n foo src`),workdir:"/w"});')
        self.assertEqual(expr, [("exec_command", {"cmd": "cd /x\nrtk rg -n foo src", "workdir": "/w"})])
        loop = extract.exec_calls('const cmds = ["rtk test bun run test a.tsx", ["tag", "rtk rg -n \\"a b\\" src"]];\n'
                                  'for (const cmd of cmds) await tools.exec_command({cmd, workdir:"/w"});')
        self.assertEqual([c[1]["cmd"] for c in loop], ["rtk test bun run test a.tsx", 'rtk rg -n "a b" src'])

    def test_reclassify_applies_current_rules(self):
        with tempfile.TemporaryDirectory() as tmp:
            conn = extract.connect(os.path.join(tmp, "census.db"))
            conn.execute("INSERT INTO calls(session, seq, tool, input_json, class, need) VALUES ('s', 0, 'Bash', ?, 'other', 'unknown')",
                         (json.dumps({"command": "rtk wc -l src/a.rs"}),))
            conn.commit()
            self.assertEqual(extract.reclassify(conn), 1)
            self.assertEqual(conn.execute("SELECT class, need FROM calls").fetchone(), ("list", "list-files"))
            self.assertEqual(extract.reclassify(conn), 0)

    def test_pi_session(self):
        session = extract.parse_pi(self.path("pi.jsonl", pi_session()), "pi-history:p1")
        self.assertEqual(len(session["calls"]), 1)
        call = session["calls"][0]
        self.assertEqual((call["tool"], call["class"], call["need"], call["output"]), ("search", "search", "find-usages", "hit"))
        self.assertEqual((session["tokens_in"], session["tokens_out"]), (8, 2))
        self.assertTrue(session["prompts"][0]["organic"])


class PromptFilter(unittest.TestCase):
    def test_kinds(self):
        self.assertEqual(extract.prompt_kind("fix the flaky retry test in upload")[0], "organic")
        self.assertEqual(extract.prompt_kind("yes do it")[0], "boundary")
        for text in ("MODE: SUBAGENT READ-WRITE. GOAL: x", "<command-name>/clear</command-name>", "/model opus", "You are a routing auditor for x y",
                     "<environment_context>\n<cwd>/x</cwd>\n</environment_context>", "", "Caveat: the messages below were generated"):
            self.assertIsNone(extract.prompt_kind(text)[0], text)
        ide = "# Context from my IDE setup:\n## Open tabs: a.rs\n## My request for Codex:\nrename the helper in the parser module"
        self.assertEqual(extract.prompt_kind(ide)[1], "rename the helper in the parser module")
        self.assertEqual(extract.prompt_kind("[Image #1] why does this layout overflow on mobile")[0], "organic")


class Ingest(unittest.TestCase):
    def test_census_ingest_is_idempotent_and_reingests_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            extract.RAW_DIR = os.path.join(tmp, "raw")
            os.makedirs(extract.RAW_DIR)
            path = os.path.join(extract.RAW_DIR, "abc.stream.jsonl")
            write_jsonl(path, claude_stream()[:3])
            conn = extract.connect(os.path.join(tmp, "census.db"))
            self.assertEqual(extract.ingest_census(conn, verbose=False), (1, 1, 2))
            self.assertEqual(extract.ingest_census(conn, verbose=False), (1, 0, 0))  # unchanged: skipped
            self.assertEqual(conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0], 0)  # census sessions belong to lane A
            with open(path, "a", encoding="utf-8") as handle:  # the run goes on
                for record in claude_stream()[3:]:
                    handle.write(json.dumps(record) + "\n")
            self.assertEqual(extract.ingest_census(conn, verbose=False), (1, 1, 3))
            rows = conn.execute("SELECT seq, tool, class, need, turn FROM calls WHERE session='abc' ORDER BY seq").fetchall()
            self.assertEqual(rows, [(0, "Grep", "search", "find-definition", 0), (1, "Read", "read", "read-region", 0), (2, "Edit", "edit", "edit-code", 0)])

    def test_summary_runs_on_a_small_database(self):
        with tempfile.TemporaryDirectory() as tmp:
            extract.RAW_DIR = os.path.join(tmp, "raw")
            os.makedirs(extract.RAW_DIR)
            write_jsonl(os.path.join(extract.RAW_DIR, "abc.stream.jsonl"), claude_stream())
            conn = extract.connect(os.path.join(tmp, "census.db"))
            extract.ingest_census(conn, verbose=False)
            import io
            out = io.StringIO()
            extract.summary(conn, out)
            text = out.getvalue()
            self.assertIn("census", text)
            self.assertIn("find-definition", text)
            self.assertNotIn("fn main", text)  # aggregates only


if __name__ == "__main__":
    unittest.main()
