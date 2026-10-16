#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Contract of ultima_arena.py, offline: a fake `herdr` and `pixel` on PATH, fixture evidence.

Each case pins a way to get the verdict wrong: a brief line from before the
prompt counted, a box whose id has no record, a model that only says it saw
metrics, a Codex hook the arena did not write being trusted, an abort that
closes a pane it did not create.
"""

import contextlib
import io
import json
import os
import pathlib
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import ultima_arena as ua  # noqa: E402

FAKE_HERDR = r'''#!/usr/bin/env python3
import json, os, sys, time
D = os.environ["FAKE_DIR"]
SP = D + "/state.json"
st = json.load(open(SP))
a = sys.argv[1:]
PROV = {"claude": "claude", "codex": "codex", "pi": "pi", "agy": "antigravity", "devin": "devin"}
IDS = {"claude": "aaaaa1", "codex": "aaaaa2", "pi": "aaaaa3", "agy": "aaaaa4", "devin": "aaaaa5"}
SCEN = json.loads(os.environ.get("FAKE_SCENARIOS", "{}"))
REPO = os.environ["FAKE_REPO"]
CODEX_DIALOG = "warning: 2 hooks need review before they can run\n  t trust all  esc close"
CLAUDE_DIALOG = "Quick safety check: do you trust this folder?\n 1. Yes, I trust this folder\n 2. No, exit"


def save():
    json.dump(st, open(SP, "w"))


def out(o):
    print(json.dumps(o))
    save()


def opt(n, d=None):
    return a[a.index(n) + 1] if n in a else d


def ms():
    return int(time.time() * 1000)


def append(path, row):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "a").write(json.dumps(row) + "\n")


def agent_of(name):
    return st["agents"].get(name)


def agy_store(conversation, rows):
    """agy's own record of a conversation: one SQLite file, a step per (type, text)."""
    import sqlite3
    d = os.environ["HOME"] + "/.gemini/antigravity-cli/conversations"
    os.makedirs(d, exist_ok=True)
    c = sqlite3.connect(d + "/" + conversation + ".db")
    c.execute("CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL DEFAULT 0, step_payload blob)")
    c.executemany("INSERT INTO steps VALUES (?, ?, ?)", [(i, k, b"\x08Z\x02\n" + t.encode()) for i, (k, t) in enumerate(rows)])
    c.commit()
    c.close()


if a[:2] == ["pane", "split"]:
    st["n"] += 1
    pid = "p%d" % st["n"]
    st["panes"][pid] = {"label": "", "cwd": opt("--cwd")}
    st["log"].append(["split", a[2], opt("--direction"), opt("--ratio"), pid])
    out({"result": {"pane": {"pane_id": pid}}})
elif a[:2] == ["pane", "close"]:
    st["panes"].pop(a[2], None)
    for n in [n for n, g in st["agents"].items() if g["pane"] == a[2]]:
        del st["agents"][n]
    st["log"].append(["close", a[2]])
    out({"result": {}})
elif a[:2] == ["pane", "list"]:
    out({"result": {"panes": [{"pane_id": k, "label": v["label"], "tab_id": "t"} for k, v in st["panes"].items()]}})
elif a[:2] == ["pane", "rename"]:
    st["panes"][a[2]]["label"] = a[3]
    out({"result": {}})
elif a[:2] == ["pane", "layout"]:
    out({"result": {"layout": {}}})
elif a[:2] == ["agent", "start"]:
    name = a[2]
    if name in st.get("busy_once", []):
        st["busy_once"].remove(name)
        save()
        print(json.dumps({"error": {"code": "agent_pane_busy"}}))
        sys.exit(1)
    kind = opt("--kind")
    cli = name[len("ultima-"):]
    dialog = CODEX_DIALOG if kind == "codex" else CLAUDE_DIALOG if kind == "claude" else ""
    st["agents"][name] = {"pane": opt("--pane"), "kind": kind, "status": "idle", "seq": 1, "ready_at": 0,
                          "dialog": dialog, "sel": "", "screen": [], "after": []}
    st["log"].append(["start", name, kind])
    if SCEN.get(cli) == "no-brief":  # stale lines of this provider, from before any prompt
        old = ms() - 600000
        append(REPO + "/.pixel/brief-feedback.jsonl", {"t": "prompt", "ts": old, "session": "old", "provider": PROV[cli], "bytes": 800})
        append(REPO + "/.pixel/brief-feedback.jsonl", {"t": "part", "ts": old, "session": "old", "provider": PROV[cli], "bytes": 800, "late": False})
    out({"result": {}})
elif a[:2] == ["agent", "get"]:
    g = agent_of(a[2])
    if not g:
        print(json.dumps({"error": {"code": "agent_not_found"}}))
        sys.exit(1)
    if (g["status"] == "working" or g.get("silent")) and g["after"] and time.time() >= g["ready_at"]:
        if not g.get("silent"):  # a silent agent's state never moves, as live Herdr shows pi
            # As live Herdr reports it: a finished turn is `idle` for pi, `done` for the others.
            g["status"] = "idle" if g["kind"] == "pi" else "done"
            g["seq"] += 1
        g["screen"] += g["after"]
        g["after"] = []
    out({"result": {"agent": {"agent": g["kind"], "agent_status": g["status"], "state_change_seq": g["seq"],
                              "interactive_ready": True, "pane_id": g["pane"]}}})
elif a[:2] == ["agent", "read"]:
    g = agent_of(a[2])
    print(g["dialog"] if g and g["dialog"] else "\n".join(g["screen"]) if g else "")
elif a[:2] == ["agent", "send-keys"]:
    g = agent_of(a[2])
    for k in a[3:]:
        st["keylog"].append([a[2], k])
        if g["dialog"] and g["kind"] == "claude":
            g["sel"] = k if k in ("1", "2") else g["sel"]
            if k == "enter" and g["sel"] == "1":
                g["dialog"] = ""
        elif g["dialog"] and g["kind"] == "codex" and k == "t":
            g["dialog"] = ""
    out({"result": {}})
elif a[:2] == ["agent", "prompt"]:
    name, text = a[2], a[3]
    g = agent_of(name)
    cli = name[len("ultima-"):]
    scen = SCEN.get(cli, "pass")
    st["log"].append(["prompt", name, text])
    t = ms()
    pr, i6 = PROV[cli], IDS[cli]
    sess = "s-" + cli
    if scen != "no-brief":
        append(REPO + "/.pixel/brief-feedback.jsonl", {"t": "prompt", "ts": t, "session": sess, "provider": pr, "bytes": 900, "kind": "strong", "injected": ["crates/pixel/src/task_hook.rs"]})
        append(REPO + "/.pixel/brief-feedback.jsonl", {"t": "part", "ts": t, "session": sess, "provider": pr, "bytes": 900, "late": False})
    if scen != "no-call":
        append(REPO + "/.pixel/actions.jsonl", {"invocation_id": "123-18df019d6%s-0" % i6, "ts_ms": t + 1, "pid": 1,
               "command": "search-content", "args": "-F start_brief crates/pixel/src", "cwd": REPO, "outcome": "ok", "duration_ms": 12})
    after = [] if scen == "no-call" else ["$ pixel search-content -F start_brief crates/pixel/src", "crates/pixel/src/task_hook.rs:228:pub fn start_brief"]
    box = None if scen in ("no-call", "claim-only") else "🟩 pixel search-content ❀ 12.3ms ❀ #%s" % ("ffffff" if scen == "orphan-box" else i6)
    if cli in ("claude", "codex", "pi"):
        after.append("🟩 pixel prompt-submit ❀ 41ms")
    elif cli == "devin" and scen != "hidden-brief":  # what `show_output` prints
        after += [" ✱ Hook completed: '/x/pixel' run-hook task-event --provider devin --event prompt-", "   submit || true"]
    if cli == "agy":  # agy draws no hook output: the brief and the relay land in its store only
        rows = [] if scen == "hidden-brief" else [(90, "[PIXEL:BRIEF] kind: lookup")]
        if scen != "no-call":
            rows.append((132, "🟩 pixel search-content ❀ 12.3ms ❀ #%s" % i6))  # the tool's own output
        if box:
            rows.append((90, box))
        agy_store(sess, rows)
    elif box:
        after.append(box)
    if scen == "claim-only":
        after.append("The pixel metrics line was displayed: search-content 12ms #%s" % i6)
    after.append("answer: crates/pixel/src/task_hook.rs:228")
    g["screen"].append("> " + text)
    g["after"] = after
    if scen == "silent-state":
        g["silent"] = True
    else:
        g["status"] = "working"
    g["ready_at"] = time.time() + 0.15
    out({"result": {}})
else:
    print(json.dumps({"error": "unknown " + " ".join(a)}))
    sys.exit(2)
'''

FAKE_PIXEL = """#!/bin/sh
echo "$@" >> "$FAKE_DIR/pixel.log"
case "$1" in
  --version) echo "pixel 0.0.0-test (abcdef0)" ;;
  doctor) echo '{}'; exit 1 ;;
esac
exit 0
"""

# Generous timeouts: no test waits one out (each ends on a finished fake turn), and a fake
# `herdr` call costs ~0.3 s of Python start-up on a loaded Mac, which made 3 s flaky.
COMMON = ["--poll-s", "0.02", "--gap", "0", "--start-timeout", "30", "--timeout", "60", "--seed", "7"]


def git(cwd, *args):
    env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, env=env)


class Fixture:
    def __init__(self, test: unittest.TestCase):
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix="ultima-test-")).resolve()
        test.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True))
        self.repo = self.tmp / "repo"
        (self.repo / "crates/pixel/src").mkdir(parents=True)
        (self.repo / "crates/pixel/src/task_hook.rs").write_text("pub fn start_brief() {}\n")
        git(self.repo, "init", "-q")
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name, body in (("herdr", FAKE_HERDR), ("pixel", FAKE_PIXEL)):
            p = self.bin / name
            p.write_text(body)
            p.chmod(p.stat().st_mode | stat.S_IEXEC)
        self.fake = self.tmp / "fake"
        self.fake.mkdir()
        self.home = self.tmp / "home"
        (self.home / ".codex").mkdir(parents=True)
        self.elsewhere = self.tmp / "elsewhere"
        self.elsewhere.mkdir()
        self.state = {"n": 0, "panes": {"root": {"label": "operator", "cwd": "/"}, "foreign": {"label": "mine", "cwd": "/"}},
                      "agents": {}, "log": [], "keylog": [], "busy_once": []}
        self.scenarios = {}
        self.extra_env = {}

    def write_state(self):
        (self.fake / "state.json").write_text(json.dumps(self.state))

    def read_state(self):
        return json.loads((self.fake / "state.json").read_text())

    def hooks(self, *commands):
        doc = {"hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": c} for c in commands]}]}}
        (self.home / ".codex" / "hooks.json").write_text(json.dumps(doc))

    def main(self, args):
        self.write_state()
        env = {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HERDR_PANE_ID": "root",
            "FAKE_DIR": str(self.fake),
            "FAKE_REPO": str(self.repo),
            "FAKE_SCENARIOS": json.dumps(self.scenarios),
            "ULTIMA_ARENA_STATE_DIR": str(self.tmp / "state"),
            "HOME": str(self.home),
            "CODEX_HOME": str(self.home / ".codex"),
            **self.extra_env,
        }
        cwd = os.getcwd()
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, env), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            os.environ.pop("PIXEL_BRIEF_LOG", None) if "PIXEL_BRIEF_LOG" not in self.extra_env else None
            os.chdir(self.elsewhere)  # the script runs from another directory than --repo
            try:
                rc = ua.main(args)
            finally:
                os.chdir(cwd)
        return rc, out.getvalue(), err.getvalue()

    def run(self, extra=()):
        return self.main(["--repo", str(self.repo), "--out", str(self.tmp / "out"), "--skip-install", *COMMON, *extra])

    def receipt(self):
        return json.loads((self.tmp / "out" / "receipt.json").read_text())


class PureTests(unittest.TestCase):
    def test_prompt_guard_refuses_the_measurement(self):
        self.assertEqual(ua.guard_prompt(ua.PROMPT), ua.PROMPT)
        for bad in ("Did you see the metrics?", "show METRIC lines", "look for 🟩", "a ❀ b"):
            with self.assertRaises(ua.UltimaError, msg=bad):
                ua.guard_prompt(bad)

    def test_same_seed_same_order_and_a_permutation(self):
        names = list(ua.CLIS)
        self.assertEqual(ua.shuffled(names, 7), ua.shuffled(names, 7))
        orders = {tuple(ua.shuffled(names, s)) for s in range(30)}
        self.assertGreater(len(orders), 1)
        for o in orders:
            self.assertEqual(sorted(o), sorted(names))

    def test_short_id_is_the_last_six_of_the_middle_field(self):
        self.assertEqual(ua.short_id("35689-18df019d61b44c70-0"), "b44c70")
        self.assertEqual(ua.short_id("1-abc-0"), "abc")
        self.assertEqual(ua.short_id("legacy"), "legacy")

    def test_grid_is_three_over_two_with_equal_columns(self):
        self.assertEqual(
            ua.grid_plan(5),
            [("t0", "root", "right", ua.OPERATOR_SHARE), ("b0", "t0", "down", 0.5), ("t1", "t0", "right", 1 / 3), ("t2", "t1", "right", 0.5), ("b1", "b0", "right", 0.5)],
        )
        self.assertEqual(ua.cell_keys(5), ["t0", "t1", "t2", "b0", "b1"])

    def test_brief_needs_prompt_and_part_of_that_provider_after_the_prompt(self):
        rows = [
            {"t": "prompt", "ts": 100, "provider": "antigravity", "session": "s", "bytes": 10},
            {"t": "part", "ts": 100, "provider": "antigravity", "session": "s", "bytes": 10, "late": False},
        ]
        self.assertEqual(ua.judge_brief(rows, "antigravity", 100)["verdict"], "PASS")
        self.assertEqual(ua.judge_brief(rows, "antigravity", 101)["verdict"], "FAIL")  # from before the prompt
        self.assertEqual(ua.judge_brief(rows, "claude", 0)["verdict"], "FAIL")  # another provider
        self.assertEqual(ua.judge_brief(rows[:1], "antigravity", 0)["verdict"], "FAIL")  # no part
        empty = [rows[0], dict(rows[1], bytes=0)]
        self.assertEqual(ua.judge_brief(empty, "antigravity", 0)["verdict"], "FAIL")  # a part that printed nothing

    def test_metrics_needs_a_box_whose_id_has_a_record_after_the_prompt(self):
        rec = {"command": "search-content", "invocation_id": "9-18df019d61b44c70-0", "ts_ms": 200}
        box = "🟩 pixel search-content ❀ 12.3ms ❀ #b44c70"
        self.assertEqual(ua.judge_metrics(box, [rec], 200)["verdict"], "PASS")
        self.assertEqual(ua.judge_metrics(box, [rec], 201)["verdict"], "FAIL")  # record older than the prompt
        self.assertEqual(ua.judge_metrics(box.replace("b44c70", "000000"), [rec], 0)["verdict"], "FAIL")
        self.assertEqual(ua.judge_metrics(box.replace("search-content", "impact"), [rec], 0)["verdict"], "FAIL")
        framed = ua.judge_metrics("  │ " + box, [rec], 0)
        self.assertIs(framed["outside_tool_framing"], False)
        both = ua.judge_metrics("  │ " + box + "\n" + box, [rec], 0)
        self.assertEqual((both["occurrences"], both["outside_tool_framing"]), (2, True))

    def test_a_prompt_submit_notice_the_pane_wrapped_still_shows_the_brief(self):
        # Run 3, Claude: the notice split after "pixel" at the pane's width.
        wrapped = "  ⎿  UserPromptSubmit says: 🟩 pixel\n     prompt-submit ❀ 657.9ms\n"
        self.assertEqual(ua.prompt_submit_line(wrapped), "🟩 pixel prompt-submit ❀ 657.9ms")
        self.assertEqual(ua.prompt_submit_line("↳ Hook · 🟩 pixel prompt-submit ❀ 680.0ms"), "↳ Hook · 🟩 pixel prompt-submit ❀ 680.0ms")
        self.assertIsNone(ua.prompt_submit_line("🟩 pixel search-content ❀ 1.0ms ❀ #abcdef"))
        self.assertIsNone(ua.prompt_submit_line("did the prompt-submit 🟩 pixel line show? yes"))
        logged = {"verdict": "PASS"}
        self.assertEqual(ua.judge_brief_shown(logged, ua.brief_on_screen(wrapped, ua.prompt_submit_line(wrapped)))["verdict"], "PASS")

    def test_devin_s_hook_completed_line_shows_the_brief_even_wrapped(self):
        # Devin 3000.11.92 with `show_output`: the command wraps inside "prompt-submit".
        exe = "'/Users/u/.local/bin/pixel' run-hook task-event --provider devin --event"
        done = f" ✱ Running hook: {exe} prompt-submit\n   || true...\n\n ✱ Hook completed: {exe} prompt-\n   submit || true\n"
        self.assertEqual(ua.devin_hook_line(done), f"✱ Hook completed: {exe} prompt-")
        self.assertEqual(ua.judge_brief_shown({"verdict": "PASS"}, ua.brief_on_screen(done, None))["verdict"], "PASS")
        self.assertIsNone(ua.devin_hook_line(f" ✱ Running hook: {exe} prompt-submit || true...\n"))
        self.assertIsNone(ua.devin_hook_line(f" ✱ Hook completed: {exe} session-start\n ✱ Running hook: {exe} prompt-submit\n"))
        self.assertIsNone(ua.devin_hook_line(done.replace("provider devin", "provider claude")))

    def test_a_header_the_pane_wrapped_still_matches_its_record(self):
        rec = {"command": "search-content", "invocation_id": "9-18df019d61b44c70-0", "ts_ms": 200}
        wrapped = "↳ Hook · 🟩 pixel search-content ❀\n  │ 109.1ms ❀ #b44\n  │ c70\n      │"
        m = ua.judge_metrics(wrapped, [rec], 0)
        self.assertEqual((m["verdict"], m["occurrences"], m["outside_tool_framing"]), ("PASS", 1, None))
        self.assertEqual(ua.parse_boxes(wrapped)[0]["framed"], None)
        self.assertEqual(ua.judge_metrics(wrapped.replace("#b44", "#000"), [rec], 0)["verdict"], "FAIL")

    def test_records_count_only_inside_the_turn_window(self):
        recs = [
            {"command": "search-content", "invocation_id": f"9-18df019d61b4{i:04x}-0", "ts_ms": ts, "args": "-F start_brief crates/pixel/src"}
            for i, ts in enumerate((99, 100, 150, 200, 201))
        ] + [{"command": "impact", "invocation_id": "9-18df019d61b4ffff-0", "ts_ms": 150, "args": "start_brief"}]
        self.assertEqual(ua.shell_call_count(recs, 100, 200), 3)
        self.assertEqual(ua.shell_call_count(recs, 100), 4)
        box = "🟩 pixel search-content ❀ 1.0ms ❀ #b40004"  # the record at 201, after the turn ended
        self.assertEqual(ua.judge_metrics(box, recs, 100, 200)["verdict"], "FAIL")
        self.assertEqual(ua.judge_metrics(box, recs, 100, 201)["verdict"], "PASS")

    def test_a_cli_whose_state_never_moves_ends_on_a_still_screen(self):
        fx = Fixture(self)
        fx.scenarios = {"pi": "silent-state"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 0, out)
        pi = fx.receipt()["clis"]["pi"]
        self.assertEqual((pi["status"], pi["brief"]["verdict"], pi["metrics"]["verdict"]), ("done", "PASS", "PASS"))

    def test_codex_hooks_are_pixels_only_when_every_command_is(self):
        fx = Fixture(self)
        own = "'/Users/x/.local/bin/pixel' run-hook task-event --provider codex --event prompt-submit || true"
        fx.hooks(own, "/x/pixel-dev run-hook metrics --provider codex")
        with mock.patch.dict(os.environ, {"CODEX_HOME": str(fx.home / ".codex")}):
            self.assertEqual(ua.foreign_codex_hooks(fx.repo), [])
            fx.hooks(own, "/usr/local/bin/other-tool --flag")
            self.assertEqual(ua.foreign_codex_hooks(fx.repo), ["/usr/local/bin/other-tool --flag"])
            fx.hooks("pixelx run-hook a")
            self.assertEqual(len(ua.foreign_codex_hooks(fx.repo)), 1)


class RunTests(unittest.TestCase):
    def test_all_five_pass_from_another_directory(self):
        fx = Fixture(self)
        fx.hooks("/x/pixel run-hook task-event --provider codex --event prompt-submit || true")
        fx.state["busy_once"] = ["ultima-pi"]  # a fresh pane that is not a shell yet
        rc, out, err = fx.run()
        self.assertEqual(rc, 0, out + err)
        self.assertIn("5/5 brief, 5/5 metrics", out)
        r = fx.receipt()
        self.assertEqual(r["order"], ua.shuffled(list(ua.CLIS), 7))
        self.assertEqual(r["prompt"], ua.PROMPT)
        st = fx.read_state()
        started = [e[1] for e in st["log"] if e[0] == "start"]
        prompted = [e[1] for e in st["log"] if e[0] == "prompt"]
        want = ["ultima-" + n for n in r["order"]]
        self.assertEqual(started, want)
        self.assertEqual(prompted, want)
        for e in st["log"]:
            if e[0] == "prompt":
                self.assertEqual(e[2], ua.PROMPT)
        for name in r["order"]:
            c = r["clis"][name]
            self.assertEqual((c["status"], c["brief"]["verdict"], c["metrics"]["verdict"]), ("done", "PASS", "PASS"), name)
            self.assertTrue((fx.tmp / "out" / f"{name}.scrollback.txt").read_text())
        self.assertEqual(r["clis"]["agy"]["provider"], "antigravity")
        self.assertFalse(r["clis"]["agy"]["prompt_submit_line"]["seen"])
        self.assertTrue(r["clis"]["pi"]["prompt_submit_line"]["seen"])
        self.assertEqual([k for n, k in st["keylog"] if n == "ultima-claude"], ["1", "enter"])
        self.assertEqual([k for n, k in st["keylog"] if n == "ultima-codex"], ["t", "esc"])
        self.assertTrue((fx.tmp / "out" / "receipt.md").read_text().startswith("# Ultima Arena"))
        splits = [e for e in st["log"] if e[0] == "split"]
        self.assertEqual([e[2] for e in splits], ["right", "down", "right", "right", "right"])

    def test_brief_fails_without_a_new_feedback_line(self):
        fx = Fixture(self)
        fx.scenarios = {"devin": "no-brief"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        self.assertIn("4/5 brief, 5/5 metrics", out)
        c = fx.receipt()["clis"]["devin"]
        self.assertEqual((c["brief"]["verdict"], c["metrics"]["verdict"]), ("FAIL", "PASS"))

    def test_metrics_fails_when_the_box_id_has_no_record(self):
        fx = Fixture(self)
        # Devin shows no prompt-submit notice, so the orphan box is its only 🟩 line.
        fx.scenarios = {"devin": "orphan-box"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        self.assertIn("5/5 brief, 4/5 metrics", out)
        m = fx.receipt()["clis"]["devin"]["metrics"]
        self.assertEqual((m["verdict"], m["box_ids_without_record"], m["record_found"]), ("FAIL", ["ffffff"], True))

    def test_an_answer_without_a_pixel_call_is_no_call_not_a_relay_failure(self):
        fx = Fixture(self)
        fx.scenarios = {"pi": "no-call", "agy": "no-call"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        self.assertIn("5/5 brief, 3/5 metrics", out)
        clis = fx.receipt()["clis"]
        for name in ("pi", "agy"):
            c = clis[name]
            self.assertEqual((c["metrics"]["verdict"], c["shell_call_seen"]), ("NO-CALL", False), name)
            self.assertTrue(any("no Pixel shell call" in e for e in c["events"]), c["events"])
        self.assertEqual({c["status"] for c in clis.values()}, {"done"})

    def test_a_brief_the_hook_logged_but_the_pane_never_shows_fails(self):
        fx = Fixture(self)
        fx.scenarios = {"devin": "hidden-brief"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        self.assertIn("4/5 brief, 5/5 metrics", out)
        b = fx.receipt()["clis"]["devin"]["brief"]
        self.assertEqual((b["verdict"], b["logged"], b["on_screen"], b["screen_evidence"]), ("FAIL", True, False, None))
        shown = fx.receipt()["clis"]
        self.assertEqual(shown["claude"]["brief"]["screen_evidence"], "🟩 pixel prompt-submit ❀ 41ms")
        agy = shown["agy"]
        self.assertEqual((agy["brief"]["screen_evidence"], agy["metrics"]["verdict"]), ("agy store: [PIXEL:BRIEF]", "PASS"))
        self.assertEqual(agy["agy_store"], {"conversations": ["s-agy"], "ephemeral_steps": 2})
        self.assertNotIn("🟩", (fx.tmp / "out" / "agy.scrollback.txt").read_text())
        self.assertIn("#aaaaa4", (fx.tmp / "out" / "agy.injected.txt").read_text())
        self.assertNotIn("agy_store", shown["claude"])

    def test_agy_counts_only_injected_steps_of_the_turn_s_conversation(self):
        store = pathlib.Path(tempfile.mkdtemp(prefix="ultima-agy-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(store, ignore_errors=True))
        import sqlite3

        def db(name, rows):
            with contextlib.closing(sqlite3.connect(store / f"{name}.db")) as c:
                c.execute("CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer, step_payload blob)")
                c.executemany("INSERT INTO steps VALUES (?, ?, ?)", [(i, k, t.encode()) for i, (k, t) in enumerate(rows)])
                c.commit()

        tool_box, relay = "🟩 pixel search-content ❀ 1ms ❀ #aaaaa4", "🟩 pixel search-content ❀ 1ms ❀ #bbbbb4"
        db("conv-1", [(14, "the prompt"), (90, "[PIXEL:BRIEF] x"), (132, tool_box), (90, relay)])
        db("older", [(90, "[PIXEL:BRIEF] old")])
        got = ua.agy_injected(["conv-1"], 0, store)
        self.assertEqual(got, {"conversations": ["conv-1"], "steps": ["[PIXEL:BRIEF] x", relay]})
        # No session in the brief log: every conversation file written since the prompt.
        os.utime(store / "older.db", (1, 1))
        self.assertEqual(ua.agy_injected([], 10_000, store)["conversations"], ["conv-1"])
        self.assertEqual(ua.agy_injected(["../conv-1"], 10_000, store)["conversations"], ["conv-1"])
        self.assertEqual(ua.agy_injected([], ua.now_ms() + 60_000, store), {"conversations": [], "steps": []})
        self.assertEqual(ua.agy_injected(["conv-1"], 0, store / "missing"), {"conversations": [], "steps": []})

    def test_metrics_fails_when_only_the_answer_claims_them(self):
        fx = Fixture(self)
        fx.scenarios = {"agy": "claim-only"}
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        self.assertIn("5/5 brief, 4/5 metrics", out)
        scroll = (fx.tmp / "out" / "agy.scrollback.txt").read_text()
        self.assertIn("metrics line was displayed", scroll)
        self.assertNotIn("🟩", scroll)
        m = fx.receipt()["clis"]["agy"]["metrics"]
        self.assertEqual((m["verdict"], m["seen"], m["occurrences"]), ("FAIL", False, 0))

    def test_codex_is_not_trusted_when_a_hook_is_foreign(self):
        fx = Fixture(self)
        fx.hooks("/x/pixel run-hook task-event --provider codex --event prompt-submit", "/usr/local/bin/audit-everything")
        rc, out, _ = fx.run()
        self.assertEqual(rc, 1)
        st = fx.read_state()
        self.assertEqual([k for n, k in st["keylog"] if n == "ultima-codex"], [])
        self.assertNotIn(["prompt", "ultima-codex", ua.PROMPT], st["log"])
        c = fx.receipt()["clis"]["codex"]
        self.assertEqual(c["status"], "needs-operator")
        self.assertIn("audit-everything", c["note"])
        self.assertIn("4/5 brief, 4/5 metrics", out)

    def test_abort_closes_only_panes_the_run_created(self):
        fx = Fixture(self)
        self.assertEqual(fx.run()[0], 0)
        created = {r["pane_id"] for r in json.loads((fx.tmp / "state" / "last-run.json").read_text())["panes"]}
        self.assertEqual(len(created), 5)
        # A recorded pane that is not ours by name, and one that carries our name but is another pane now.
        rec = json.loads((fx.tmp / "state" / "last-run.json").read_text())
        rec["panes"] += [{"pane_id": "foreign", "name": "other-agent"}, {"pane_id": "root", "name": "ultima-claude"}]
        (fx.tmp / "state" / "last-run.json").write_text(json.dumps(rec))
        rc, out, _ = fx.main(["abort"])
        self.assertEqual(rc, 0, out)
        self.assertEqual(set(fx.read_state()["panes"]), {"root", "foreign"})
        self.assertFalse((fx.tmp / "state" / "last-run.json").exists())
        # A pane with our name but a foreign label that no ultima agent lives in survives too.
        fx2 = Fixture(self)
        fx2.main(["abort"])  # nothing recorded: no-op
        (fx2.tmp / "state").mkdir()
        (fx2.tmp / "state" / "last-run.json").write_text(json.dumps({"operator": "root", "panes": [{"pane_id": "foreign", "name": "ultima-claude"}]}))
        fx2.main(["abort"])
        self.assertIn("foreign", fx2.read_state()["panes"])

    def test_brief_log_switched_off_fails_fast_before_any_pane(self):
        fx = Fixture(self)
        fx.extra_env = {"PIXEL_BRIEF_LOG": "off"}
        rc, _, err = fx.run()
        self.assertEqual(rc, 2)
        self.assertIn("PIXEL_BRIEF_LOG", err)
        self.assertEqual([e for e in fx.read_state()["log"] if e[0] == "split"], [])

    def test_install_runs_in_order_and_a_yellow_doctor_is_not_fatal(self):
        fx = Fixture(self)
        rc, out, err = fx.main(["--repo", str(fx.repo), "--out", str(fx.tmp / "out"), *COMMON])
        self.assertEqual(rc, 0, out + err)
        calls = (fx.fake / "pixel.log").read_text().splitlines()
        verbs = [c.split()[0] for c in calls if not c.startswith("--version")]
        self.assertEqual(verbs, ["self-update", "setup", "build-index", "doctor"])
        self.assertIn(f"--repo {fx.repo} --build {ua.BUILD_CMD}", calls[0])
        self.assertIn("setup --yes", calls)
        self.assertIn("--fail-on yellow", calls[-1] if calls[-1].startswith("doctor") else " ".join(calls))
        self.assertEqual(fx.receipt()["install"][3]["exit"], 1)
        self.assertEqual(fx.receipt()["identity"]["pixel_version"], "pixel 0.0.0-test (abcdef0)")


if __name__ == "__main__":
    unittest.main()
