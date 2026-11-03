#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Mine this project's local agent transcripts for the tool sequences a brief probe could replace (issue #906).

    extract-probes.py [--since 30d] [--agent all|claude|codex|pi] [--top 20] [--project PATH]
    extract-probes.py --self-test

Sources (read-only, local): Claude Code ``~/.claude/projects/<slug>/*.jsonl`` for the project and its worktrees,
Codex ``~/.codex/sessions/**/rollout-*.jsonl`` whose ``cwd`` is under the project. Pi sessions have no stable
transcript format to read here and are reported as skipped.

Each session is cut into turns: a user message, then the tool calls before the first edit or the end of the turn.
Every call is normalised to a SHAPE with paths reduced to roles (``read(code)``, ``read(doc)``, ``grep``,
``bash:git-log``, ``bash:--help``...). The report ranks shapes and sequences of 1 to 4 calls by the number of
turns they appear in times the calls they cost, grouped by the kind of question the turn asked (a keyword
heuristic, the same labels as the briefs), and names the probe that would cover each or marks it as a candidate.

Privacy: transcripts and prompts never leave this process. Only counts and abstract shapes are printed; no
prompt, path, command, output or secret is read into the report, and nothing is written to disk. Standard
library only; run with ``python3 -I``.
"""

import argparse
import collections
import json
import os
import re
import sys
import time
import unittest
from pathlib import Path

HOME = Path.home()
EDIT_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit", "apply_patch"}
DOC_EXT = (".md", ".mdx", ".rst", ".txt", ".adoc")
CONFIG_EXT = (".toml", ".yaml", ".yml", ".json", ".ini", ".cfg", ".env", ".lock", ".conf")
KINDS = (
    ("history", r"\b(which commit|when did|when was|who (changed|added|wrote)|since when|git log|blame|introduced|history)\b"),
    ("review", r"\b(review|latest commit|this (pr|diff)|risky|missing tests)\b"),
    ("flow", r"\b(walk me through|trace|flow|what happens when|from .* until|lifecycle|how does .* work)\b"),
    ("config", r"\b(config\w*|setting|env(ironment)? var\w*|flag|option|default|port|where (do|can) i set)\b"),
    ("tests", r"\b(tests?|coverage|spec)\b"),
    ("overview", r"\b(overview|tour|architecture|structure|what is this|what does this repo)\b"),
    ("lookup", r"\b(where is|where does|what is the|defined|which (function|file|struct))\b"),
)
# Shape -> the probe that covers it (a prefix match), else a candidate.
COVERED = (
    ("bash:git-log", "history-fallback"), ("bash:git-show", "history-fallback"), ("bash:git-diff", "history-fallback"),
    ("read(doc)", "doc-sections"), ("bash:--help", None), ("read(config)", "config-registry"),
    ("glob", "name-files"),
)


def kind_of(prompt):
    low = prompt.lower()
    for kind, pattern in KINDS:
        if re.search(pattern, low):
            return kind
    return "other"


def role_of(path):
    low = str(path).lower()
    name = low.rsplit("/", 1)[-1]
    if re.search(r"(^|/)(tests?|__tests__|spec)(/|$)|(_test|\.test|\.spec|test_)", low):
        return "test"
    if low.endswith(DOC_EXT) or "/docs/" in low or name.startswith(("readme", "contributing", "agents", "claude")):
        return "doc"
    if low.endswith(CONFIG_EXT) or name.startswith((".env", "dockerfile", "makefile")) or "config" in name:
        return "config"
    return "code"


def bash_shape(command, depth=0):
    words = command.strip().split()
    while words and (words[0] in ("rtk", "proxy", "env", "time", "command", "timeout", "nice") or "=" in words[0]
                     or re.fullmatch(r"\d+[smh]?", words[0])):
        words = words[1:]
    if not words:
        return "bash:other"
    head = os.path.basename(words[0])
    if head in ("bash", "sh", "zsh") and depth < 2:
        rest = " ".join(words[1:])
        match = re.search(r"-\w*c\s+(['\"])(.*)\1\s*$", rest, re.S)
        if match:
            return bash_shape(match.group(2), depth + 1)
    if head == "run" and len(words) > 1 and depth < 2:
        return bash_shape(" ".join(words[1:]).strip("'\""), depth + 1)
    if head == "read":
        files = [w for w in words[1:] if not w.startswith("-")]
        return f"read({role_of(files[-1]) if files else 'code'})"
    text = " ".join(words)
    if "--help" in words or " -h" in text.split("|")[0][-4:]:
        return "bash:--help"
    if head == "git":
        sub = next((w for w in words[1:] if not w.startswith("-")), "")
        return f"bash:git-{sub}" if sub in ("log", "show", "diff", "blame", "status", "grep") else "bash:git-other"
    if head in ("rg", "grep", "ag", "ack"):
        return "grep"
    if head in ("sed", "cat", "head", "tail", "nl", "bat"):
        files = [w for w in words[1:] if "/" in w or "." in w.rsplit("/", 1)[-1]]
        return f"read({role_of(files[-1]) if files else 'code'})"
    if head in ("ls", "find", "fd", "tree", "wc"):
        return "glob"
    if head in ("cargo", "pytest", "npm", "bun", "yarn", "pnpm", "go", "make", "just"):
        return "bash:build-test"
    if head in ("pixel", "pixel-dev"):
        return "bash:pixel"
    if head in ("gh", "but", "herdr", "git-butler"):
        return f"bash:{head}"
    if head in ("python3", "python", "node", "bunx", "uv"):
        return "bash:script"
    if head in ("curl", "wget"):
        return "bash:web"
    if head in ("mkdir", "cp", "mv", "rm", "touch", "chmod", "ln"):
        return "bash:fs"
    if head in ("sqlite3", "jq", "yq"):
        return "bash:data"
    if head in ("echo", "printf", "sleep", "cd", "pwd", "true", "which", "command"):
        return "bash:shell"
    return "bash:other"


def shape_of_claude(name, inp):
    if name in ("Read",):
        return f"read({role_of(inp.get('file_path', ''))})"
    if name in ("Grep",):
        return "grep"
    if name in ("Glob", "LS"):
        return "glob"
    if name == "Bash":
        return bash_shape(str(inp.get("command") or ""))
    if name in ("WebFetch", "WebSearch"):
        return "web"
    if name in ("Task", "Agent"):
        return "subagent"
    return f"tool:{name.lower()}" if not name.startswith("mcp__") else "mcp"


def text_of(content):
    if isinstance(content, str):
        return content
    return " ".join(p.get("text", "") for p in content
                    if isinstance(p, dict) and p.get("type") in ("text", "input_text", "output_text"))


def claude_turns(path):
    """``[(kind, [shape])]`` of one Claude Code transcript: calls between a user message and the first edit."""
    turns, kind, shapes, open_ = [], None, [], False
    for line in Path(path).read_text(errors="replace").splitlines():
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        message = rec.get("message") or {}
        if rec.get("type") == "user" and not rec.get("isSidechain"):
            content = message.get("content")
            if isinstance(content, list) and any(p.get("type") == "tool_result" for p in content if isinstance(p, dict)):
                continue
            prompt = text_of(content or "")
            if prompt.strip() and not prompt.lstrip().startswith(("<", "Caveat:")):
                if kind is not None and shapes:
                    turns.append((kind, shapes))
                kind, shapes, open_ = kind_of(prompt), [], True
        elif rec.get("type") == "assistant" and open_ and not rec.get("isSidechain"):
            for part in message.get("content") or []:
                if isinstance(part, dict) and part.get("type") == "tool_use":
                    if part.get("name") in EDIT_TOOLS:
                        open_ = False
                        break
                    shapes.append(shape_of_claude(part.get("name", "?"), part.get("input") or {}))
    if kind is not None and shapes:
        turns.append((kind, shapes))
    return turns


HEADS = r"(?:rtk |git |rg |sed |cat |ls |find |grep |gh |cargo |pixel |nl |head |tail |wc |python3? |jq |curl |but )"


def js_commands(source):
    """The shell commands a Codex JS ``exec`` call runs: ``cmd: "..."`` arguments and command-looking strings."""
    found = re.findall(r'cmd\s*:\s*"((?:\\.|[^"\\])*)"', source) + re.findall(rf'"({HEADS}(?:\\.|[^"\\])*)"', source)
    out = []
    for raw in found:
        try:
            text = json.loads(f'"{raw}"')
        except ValueError:
            text = raw
        if text not in out:
            out.append(text)
    return out


def codex_turns(path, project):
    """``[(kind, [shape])]`` of one Codex rollout whose cwd is under ``project`` (else nothing)."""
    turns, kind, shapes, open_, inside = [], None, [], False, False
    for line in Path(path).read_text(errors="replace").splitlines():
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        payload = rec.get("payload") or {}
        if rec.get("type") == "session_meta":
            inside = str(payload.get("cwd", "")).startswith(str(project))
            if not inside:
                return []
        if rec.get("type") != "response_item":
            continue
        if payload.get("type") == "message" and payload.get("role") == "user":
            prompt = text_of(payload.get("content") or [])
            if prompt.strip() and not re.match(r"\s*(<|# AGENTS|<environment)", prompt):
                if kind is not None and shapes:
                    turns.append((kind, shapes))
                kind, shapes, open_ = kind_of(prompt), [], True
        elif open_ and payload.get("type") in ("function_call", "local_shell_call", "custom_tool_call"):
            if payload.get("type") == "custom_tool_call" and payload.get("name") == "exec":
                # Codex's JS exec tool batches shell commands: each `cmd:"..."` or command-looking string is a call.
                commands = js_commands(str(payload.get("input") or ""))
                if any("apply_patch" in c for c in commands):
                    open_ = False
                    continue
                shapes += [bash_shape(c) for c in commands] or ["tool:js"]
                continue
            args = payload.get("arguments") or payload.get("input") or ""
            try:
                parsed = json.loads(args) if isinstance(args, str) else args
            except ValueError:
                parsed = {}
            command = ""
            if isinstance(parsed, dict):
                command = str(parsed.get("cmd") or parsed.get("command") or "")
            elif payload.get("action"):
                command = " ".join(payload["action"].get("command") or [])
            if payload.get("name") == "apply_patch" or "apply_patch" in command:
                open_ = False
                continue
            shapes.append(bash_shape(command) if command else f"tool:{payload.get('name', '?')}")
    if kind is not None and shapes:
        turns.append((kind, shapes))
    return turns


def gather(project, agent, since_days, claude_root=None, codex_root=None):
    claude_root = Path(claude_root or HOME / ".claude" / "projects")
    codex_root = Path(codex_root or HOME / ".codex" / "sessions")
    cutoff = time.time() - since_days * 86400
    turns, files = [], collections.Counter()
    slug = str(project).replace("/", "-").replace(".", "-")
    if agent in ("all", "claude") and claude_root.exists():
        for directory in sorted(claude_root.glob(f"{slug}*")):
            for path in sorted(directory.glob("*.jsonl")):
                if path.stat().st_mtime >= cutoff:
                    files["claude"] += 1
                    turns += [("claude", k, s) for k, s in claude_turns(path)]
    if agent in ("all", "codex") and codex_root.exists():
        for path in sorted(codex_root.rglob("rollout-*.jsonl")):
            if path.stat().st_mtime >= cutoff:
                found = codex_turns(path, project)
                if found:
                    files["codex"] += 1
                    turns += [("codex", k, s) for k, s in found]
    return turns, files


def ngrams(shapes, n_max=4):
    seen = set()
    for n in range(1, n_max + 1):
        for i in range(len(shapes) - n + 1):
            seen.add(tuple(shapes[i:i + n]))
    return seen


def covering(sequence):
    """The probe that covers a sequence: the one that covers every call, else the first covered call's, else None."""
    names = []
    for shape in sequence:
        hit = next((p for prefix, p in COVERED if shape.startswith(prefix)), None)
        names.append(hit)
    covered = {n for n in names if n}
    return ", ".join(sorted(covered)) if covered and all(names) else (("partly " + ", ".join(sorted(covered))) if covered else None)


def rank(turns, top, kinds=None):
    """Rows ``(kind, sequence, turns containing it, calls it costs, score, probe)`` by turns x calls."""
    count = collections.Counter()
    for _agent, kind, shapes in turns:
        if kinds and kind not in kinds:
            continue
        for gram in ngrams(shapes):
            count[(kind, gram)] += 1
        if len(shapes) > 4:
            count[(kind, tuple(shapes[:6]))] += 1   # the pre-answer sequence itself (its first six calls)
    rows = []
    for (kind, gram), n in count.items():
        if n < 2:
            continue
        rows.append((kind, gram, n, len(gram), n * len(gram), covering(gram)))
    rows.sort(key=lambda r: (-r[4], -r[3], r[0], r[1]))
    out, seen = [], set()
    for row in rows:
        # A sequence that only extends an already listed one with the same count adds nothing.
        key = (row[0], row[2], row[1][:2])
        if key in seen and row[3] > 2:
            continue
        seen.add(key)
        out.append(row)
        if len(out) >= top:
            break
    return out


def render(turns, files, rows):
    kinds = collections.Counter(k for _a, k, _s in turns)
    calls = sum(len(s) for _a, _k, s in turns)
    out = [f"{len(turns)} turns with tool calls before the first edit, {calls} calls, from "
           f"{files.get('claude', 0)} Claude and {files.get('codex', 0)} Codex transcripts "
           f"(pi: skipped). Turns by question kind: " + ", ".join(f"{k} {n}" for k, n in kinds.most_common()),
           "", "| # | kind | pattern (shapes, in order) | turns | calls | covered by |", "|---|---|---|---:|---:|---|"]
    for i, (kind, gram, n, size, _score, probe) in enumerate(rows, 1):
        out.append(f"| {i} | {kind} | {' -> '.join(gram)} | {n} | {n * size} | {probe or 'no probe: candidate'} |")
    return "\n".join(out)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--since", default="30d")
    parser.add_argument("--agent", choices=("all", "claude", "codex", "pi"), default="all")
    parser.add_argument("--top", type=int, default=20)
    parser.add_argument("--kinds", default="question",
                        help="question (every kind but `other`, the default), all, or a comma-separated list")
    parser.add_argument("--project", default=str(Path(__file__).resolve().parents[2]))
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    days = int(re.fullmatch(r"(\d+)d", args.since).group(1)) if re.fullmatch(r"\d+d", args.since) else 30
    project = Path(args.project).resolve()
    # The checkout and its sibling worktrees (pixel, pixel-wt/...) are one project for the transcripts.
    base = str(project)
    for marker in ("-wt", "/pixel-wt"):
        base = base.split(marker)[0]
    turns, files = gather(Path(base), args.agent if args.agent != "pi" else "none", days)
    if args.agent == "pi":
        print("pi: no transcript reader yet; nothing to mine")
        return 0
    wanted = None if args.kinds == "all" else ({k for k, _ in KINDS} if args.kinds == "question" else set(args.kinds.split(",")))
    print(render(turns, files, rank(turns, args.top, wanted)))
    return 0


class SelfTest(unittest.TestCase):
    def test_a_call_becomes_a_shape_with_paths_reduced_to_roles(self):
        self.assertEqual(shape_of_claude("Read", {"file_path": "/r/src/lib.rs"}), "read(code)")
        self.assertEqual(shape_of_claude("Read", {"file_path": "/r/README.md"}), "read(doc)")
        self.assertEqual(shape_of_claude("Read", {"file_path": "/r/tests/a_test.py"}), "read(test)")
        self.assertEqual(shape_of_claude("Read", {"file_path": "/r/pyproject.toml"}), "read(config)")
        self.assertEqual(shape_of_claude("Grep", {"pattern": "secret-word"}), "grep")
        self.assertEqual(shape_of_claude("Bash", {"command": "rtk git log --oneline -5"}), "bash:git-log")
        self.assertEqual(shape_of_claude("Bash", {"command": "pixel --help"}), "bash:--help")
        self.assertEqual(shape_of_claude("Bash", {"command": "sed -n 1,9p crates/a/src/x.rs"}), "read(code)")
        self.assertEqual(shape_of_claude("Bash", {"command": "rg -n foo ."}), "grep")
        self.assertEqual(shape_of_claude("Bash", {"command": "ls -la"}), "glob")

    def test_the_kind_of_a_question_comes_from_its_words(self):
        self.assertEqual(kind_of("which commit added the gate and when did it land"), "history")
        self.assertEqual(kind_of("walk me through what happens when I press undo"), "flow")
        self.assertEqual(kind_of("where do I set the port and which environment variables apply"), "config")
        self.assertEqual(kind_of("fix the typo"), "other")

    def test_a_claude_transcript_is_cut_into_turns_that_end_at_the_first_edit(self):
        import tempfile

        def line(**rec):
            return json.dumps(rec)

        def use(name, **inp):
            return line(type="assistant", message={"content": [{"type": "tool_use", "id": "x", "name": name, "input": inp}]})

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "s.jsonl"
            path.write_text("\n".join([
                line(type="user", message={"content": "where do I set the port, which config?"}),
                use("Grep", pattern="PORT"), use("Read", file_path="/r/a.env"), use("Edit", file_path="/r/a.rs"),
                use("Read", file_path="/r/after_edit.rs"),
                line(type="user", message={"content": [{"type": "tool_result", "tool_use_id": "x", "content": "ok"}]}),
                line(type="user", message={"content": "<system-reminder>ignore</system-reminder>"}),
                line(type="user", message={"content": "which commit added it"}),
                use("Bash", command="git log -S foo"), use("Bash", command="git show abc"),
            ]))
            self.assertEqual(claude_turns(path), [("config", ["grep", "read(config)"]),
                                                  ("history", ["bash:git-log", "bash:git-show"])])

    def test_a_codex_rollout_outside_the_project_is_skipped_and_inside_it_is_cut_into_turns(self):
        import tempfile

        def item(payload):
            return json.dumps({"type": "response_item", "payload": payload})

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rollout-1.jsonl"
            body = [json.dumps({"type": "session_meta", "payload": {"cwd": "/work/proj/sub"}}),
                    item({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>"}]}),
                    item({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "walk me through the flow"}]}),
                    item({"type": "function_call", "name": "exec_command", "arguments": json.dumps({"cmd": "rg -n x ."})}),
                    item({"type": "function_call", "name": "exec_command", "arguments": json.dumps({"cmd": "sed -n 1,5p src/a.py"})}),
                    item({"type": "function_call", "name": "apply_patch", "arguments": "{}"}),
                    item({"type": "function_call", "name": "exec_command", "arguments": json.dumps({"cmd": "ls"})})]
            path.write_text("\n".join(body))
            self.assertEqual(codex_turns(path, Path("/work/proj")), [("flow", ["grep", "read(code)"])])
            self.assertEqual(codex_turns(path, Path("/elsewhere")), [])

    def test_the_js_exec_tool_of_codex_yields_its_shell_commands(self):
        source = 'const r = await tools.exec_command({cmd:"rtk git log --oneline -5",workdir:"/r"});\nconst cmds = ["rtk rg -n \\"x\\" src", "sed -n 1,9p a.rs"];'
        self.assertEqual(js_commands(source), ["rtk git log --oneline -5", 'rtk rg -n "x" src', "sed -n 1,9p a.rs"])
        self.assertEqual(js_commands("text(hits);"), [])

    def test_the_ranking_counts_turns_times_calls_and_names_the_covering_probe(self):
        turns = [("claude", "history", ["bash:git-log", "bash:git-show"])] * 3 + [("codex", "config", ["grep"])] * 2 \
            + [("claude", "other", ["glob"])]
        rows = rank(turns, 10)
        top = rows[0]
        self.assertEqual((top[0], top[1], top[2], top[4]), ("history", ("bash:git-log", "bash:git-show"), 3, 6))
        self.assertEqual(top[5], "history-fallback")
        self.assertTrue(all(r[2] >= 2 for r in rows), "a pattern seen once is not a pattern")
        self.assertIn("grep", [r[1][0] for r in rows])
        self.assertEqual(covering(("grep",)), None)
        self.assertEqual(covering(("read(doc)",)), "doc-sections")
        self.assertEqual(covering(("grep", "read(doc)")), "partly doc-sections")

    def test_the_report_carries_counts_and_shapes_only(self):
        turns = [("claude", "history", ["bash:git-log"])] * 2
        text = render(turns, {"claude": 1}, rank(turns, 5))
        self.assertIn("2 turns", text)
        self.assertIn("bash:git-log", text)
        self.assertIn("history-fallback", text)


if __name__ == "__main__":
    sys.exit(main())
