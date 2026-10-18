#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Labelled replay of real prompts through the prompt-submit brief (issue #1032): does it reach the project prompts?

Two steps, the first local and private, the second the published measurement:

* ``extract`` reads the agent transcripts on this machine (Claude Code ``~/.claude/projects``, Codex
  ``~/.codex/sessions``, pi ``~/.pi/agent/sessions``) for the prompts a human typed in a time window, with the
  repository each was typed in and the repository files the agent read or edited before the next prompt. It writes
  JSONL rows ``{id, source, ts, repo, prompt, touched, follows}`` for labelling. Harness turns (slash commands,
  hook and task notifications, interrupts, ``codex exec``, sessions outside a git repository) are left out.
  The file holds prompt text: keep it out of the repository.
* ``score`` replays every labelled row (``label``: ``project``, ``follow-up`` or ``other``) through ``<bin>
  run-hook task-event --provider pi --event prompt-submit`` with a ``brief_only`` payload, in the row's repository
  as it is now (the hook's own window and budget, a fresh session per row, dedupe and logging off), and reports:

  - project recall: briefed project prompts / project prompts; follow-up recall likewise;
  - precision: briefed project and follow-up prompts / briefed prompts;
  - hit@8: of the briefed project prompts whose turn touched a file, those where one of the first 8 repository
    files the brief names is among the touched ones.

  ``report.json`` and ``report.md`` carry the counts, the binary's SHA-256, the label file's SHA-256, the window and
  the command; never a prompt. A row whose repository has no Pixel index is counted apart (``unindexed``), not as a
  miss: the replay measures the brief, not whether ``prepare-repo`` ran.

The replay runs each prompt against the repository's current tree, not the one it was typed against, and with no
session history, so a follow-up's session lift cannot apply: follow-up recall is a lower bound.

    brief-recall.py extract --since 2026-10-16T18:10 [--until ISO] --out rows.jsonl
    brief-recall.py score --labels labelled.jsonl --bin <pixel> --out DIR [--label NAME]
    brief-recall.py --self-test

Standard library only; run with ``python3 -I``.
"""

import argparse
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path

LABELS = ("project", "follow-up", "other")
HIT_AT = 8
# Text a harness puts in the user's turn: commands, hook and task notifications, interrupts, injected context.
HARNESS = re.compile(r"^\s*(<command-|<local-command|<task-notification|<system-reminder|<ci-monitor-event|"
                     r"<side-sessions-event|<user-prompt-submit-hook|<environment_context|<user_instructions|"
                     r"<external_codex_apps_|<bash-input>|<bash-stdout>|This session is being continued from a previous conversation|"
                     r"\[Request interrupted|Caveat: The messages below|# AGENTS\.md instructions)")
# Context a host prepends to what the person typed: dropped, and the rest kept.
UNSUPPORTED = re.compile(r"\[external unsupported block: [^\]]*\]")   # a copy's placeholder for an image
AMBIENT = re.compile(r"<in-app-browser-context\b.*?</in-app-browser-context>\s*", re.S)
TOUCH_KEYS = ("file_path", "path", "notebook_path")
PATH_RE = re.compile(r"(?:^|[\s`'\"(\[])((?:[\w.-]+/)*[\w.-]+\.[A-Za-z0-9]{1,8})(?::\d+(?:-\d+)?)?")


# --------------------------------------------------------------------------
# Transcripts
# --------------------------------------------------------------------------

def parse_ts(value):
    """Epoch seconds of an ISO time (``Z`` or an offset; a naive time is local)."""
    if isinstance(value, (int, float)):
        return value / 1000 if value > 1e11 else float(value)
    text = str(value).replace("Z", "+00:00")
    stamp = datetime.fromisoformat(text)
    return stamp.timestamp()


def git_root(cwd, cache):
    if cwd not in cache:
        try:
            proc = subprocess.run(["git", "-C", cwd, "rev-parse", "--show-toplevel"], capture_output=True, text=True,
                                  timeout=10)
            cache[cwd] = proc.stdout.strip() if proc.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            cache[cwd] = None
    return cache[cwd]


def text_of(content):
    """The typed text of a message's content: a string, or the text items of a list (none when it holds a tool
    result, which is the harness answering a call, not a person)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if any(isinstance(c, dict) and c.get("type") in ("tool_result", "function_call_output") for c in content):
            return ""
        return "\n".join(c.get("text", "") for c in content if isinstance(c, dict)
                         and c.get("type") in ("text", "input_text"))
    return ""


def human(text):
    return bool(text.strip()) and not HARNESS.match(text)


def typed(text):
    """What the person typed, without the ambient context a host prepends."""
    return AMBIENT.sub("", text)


def paths_in(value, root):
    """Repository-relative files named by a tool input: its path keys, else the existing files among the words of a
    shell command."""
    found = []
    if isinstance(value, dict):
        for key in TOUCH_KEYS:
            if isinstance(value.get(key), str):
                found.append(value[key])
        for key in ("command", "cmd"):
            cmd = value.get(key)
            if isinstance(cmd, list):
                cmd = " ".join(map(str, cmd))
            if isinstance(cmd, str):
                found += PATH_RE.findall(cmd)
    elif isinstance(value, str):
        found += PATH_RE.findall(value)
    out = []
    for raw in found:
        path = Path(raw) if os.path.isabs(raw) else Path(root) / raw
        try:
            rel = path.resolve().relative_to(Path(root).resolve())
        except (ValueError, OSError):
            continue
        if (Path(root) / rel).is_file():
            out.append(rel.as_posix())
    return out


class Turns:
    """Prompts of one session in order, each with the files touched until the next one."""

    def __init__(self, source, cwd):
        self.source, self.cwd, self.rows = source, cwd, []

    def prompt(self, ts, text):
        self.rows.append({"source": self.source, "ts": ts, "cwd": self.cwd, "prompt": text.strip(), "touched": []})

    def touch(self, value):
        if self.rows and self.cwd:
            self.rows[-1]["touched"] += [p for p in paths_in(value, self.cwd) if p not in self.rows[-1]["touched"]]


def claude_session(path):
    turns = None
    for line in open(path, errors="replace"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("isSidechain") or d.get("isMeta"):
            continue
        if d.get("entrypoint", "").startswith("sdk"):   # `claude -p` and SDK runs: a program, not a person
            return []
        if turns is None and d.get("cwd"):
            turns = Turns("claude", d["cwd"])
        if turns is None:
            continue
        msg = d.get("message") or {}
        if d.get("type") == "user":
            text = typed(text_of(msg.get("content")))
            if human(text):
                turns.prompt(parse_ts(d.get("timestamp")), text)
        elif d.get("type") == "assistant":
            for item in msg.get("content") or []:
                if isinstance(item, dict) and item.get("type") == "tool_use":
                    turns.touch(item.get("input"))
    return turns.rows if turns else []


def codex_session(path):
    turns = None
    for line in open(path, errors="replace"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        p = d.get("payload") or {}
        if d.get("type") == "session_meta":
            if p.get("originator") == "codex_exec" or p.get("source") == "exec":
                return []
            turns = Turns("codex", p.get("cwd"))
            continue
        if turns is None or d.get("type") != "response_item":
            continue
        if p.get("type") == "message" and p.get("role") == "user":
            text = typed(text_of(p.get("content")))
            if human(text):
                turns.prompt(parse_ts(d.get("timestamp")), text)
        elif p.get("type") in ("function_call", "custom_tool_call", "local_shell_call"):
            args = p.get("arguments") or p.get("input") or p.get("action") or {}
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except ValueError:
                    args = {"command": args}
            turns.touch(args)
    return turns.rows if turns else []


def pi_session(path):
    turns = None
    for line in open(path, errors="replace"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("type") == "session":
            turns = Turns("pi", d.get("cwd"))
            continue
        if turns is None or d.get("type") != "message":
            continue
        msg = d.get("message") or {}
        if msg.get("role") == "user":
            text = typed(text_of(msg.get("content")))
            if human(text):
                turns.prompt(parse_ts(d.get("timestamp")), text)
        elif msg.get("role") == "assistant":
            for item in msg.get("content") or []:
                if isinstance(item, dict) and item.get("type") in ("toolCall", "tool_use"):
                    turns.touch(item.get("arguments") or item.get("input"))
    return turns.rows if turns else []


SOURCES = (("~/.claude/projects", "*/*.jsonl", claude_session), ("~/.codex/sessions", "*/*/*/*.jsonl", codex_session),
           ("~/.pi/agent/sessions", "*/*.jsonl", pi_session))


def extract(since, until, home=None, temp_roots=None):
    """Human prompts typed in ``[since, until)`` inside a git repository, oldest first, with ``follows`` set when an
    earlier prompt of the same session precedes it."""
    home = Path(home or os.path.expanduser("~"))
    temp_roots = [os.path.realpath(t) for t in (temp_roots or [tempfile.gettempdir(), "/tmp", "/private/tmp"])]
    roots, rows = {}, []
    for base, pattern, reader in SOURCES:
        directory = home / base.removeprefix("~/")
        for path in sorted(directory.glob(pattern)) if directory.is_dir() else []:
            if path.stat().st_mtime < since:
                continue
            session = reader(path)
            for k, row in enumerate(session):
                if not (since <= row["ts"] < until) or not row["cwd"]:
                    continue
                if any(os.path.realpath(row["cwd"]).startswith(t) for t in temp_roots):
                    continue   # throwaway clones and scratch directories: a harness, not a project
                repo = git_root(row["cwd"], roots)
                if not repo:
                    continue
                row = dict(row, repo=repo, follows=k > 0, session=path.stem)
                row["id"] = hashlib.sha256(f"{path}:{row['ts']}:{row['prompt']}".encode()).hexdigest()[:16]
                rows.append(row)
    return dedupe(sorted(rows, key=lambda r: (r["ts"], r["id"])))


def dedupe(rows):
    """One row per prompt typed once: a resumed or forked session copies its earlier turns into a second transcript,
    so the same text in the same repository counts once, with the most touched files any copy recorded."""
    kept = {}
    for row in rows:
        key = (row["repo"], " ".join(UNSUPPORTED.sub("", row["prompt"]).split()))
        if key not in kept or len(row["touched"]) > len(kept[key]["touched"]):
            first = kept.get(key, row)
            kept[key] = dict(row, ts=min(first["ts"], row["ts"]), id=first["id"], follows=first["follows"])
    return sorted(kept.values(), key=lambda r: (r["ts"], r["id"]))


# --------------------------------------------------------------------------
# Replay
# --------------------------------------------------------------------------

def file_sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def indexed(repo):
    return (Path(repo) / ".pixel" / "base.shard").exists()


def named_files(brief, repo, limit=HIT_AT):
    """The first ``limit`` distinct repository files a brief names, in order."""
    out = []
    for raw in PATH_RE.findall(brief):
        if raw not in out and (Path(repo) / raw).is_file():
            out.append(raw)
            if len(out) == limit:
                break
    return out


def replay(binary, row, timeout=60):
    payload = json.dumps({"session_id": f"recall-{row['id']}", "cwd": row["repo"], "prompt": row["prompt"],
                          "brief_only": True})
    env = dict(os.environ, PIXEL_BRIEF_DEDUPE="0", PIXEL_BRIEF_LOG="0")
    env.pop("PIXEL_BRIEF", None)
    env.pop("ANTHROPIC_API_KEY", None)
    try:
        proc = subprocess.run([binary, "run-hook", "task-event", "--provider", "pi", "--event", "prompt-submit"],
                              input=payload, cwd=row["repo"], capture_output=True, text=True, timeout=timeout, env=env)
        return (json.loads(proc.stdout).get("context") or "") if proc.stdout.strip() else ""
    except (OSError, subprocess.SubprocessError, ValueError):
        return ""


WARM_PROMPT = "where is the main entry point of this program defined"


def warm(binary, repo, replayer, tries=10, need=3):
    """Throwaway briefs until ``need`` in a row print the same text: the first briefs after a daemon starts come back
    empty while it builds what they ask of it (a first replay pass briefed 71 of 218 rows, the next one 80)."""
    last, same = None, 0
    for k in range(tries):
        text = replayer(binary, {"id": f"warm-{k}", "repo": repo, "prompt": WARM_PROMPT})
        same = same + 1 if text == last else 1
        last = text
        if same >= need:
            return
        time.sleep(1)


def ratio(num, den):
    return None if den == 0 else num / den


def metrics(results):
    by = {label: [r for r in results if r["label"] == label] for label in LABELS}
    briefed = [r for r in results if r["briefed"]]
    relevant = [r for r in briefed if r["label"] in ("project", "follow-up")]
    hit_pool = [r for r in by["project"] if r["briefed"] and r["touched"]]
    return {
        "rows": len(results),
        "labels": {k: len(v) for k, v in by.items()},
        "briefed": len(briefed),
        "project_recall": ratio(sum(r["briefed"] for r in by["project"]), len(by["project"])),
        "follow_up_recall": ratio(sum(r["briefed"] for r in by["follow-up"]), len(by["follow-up"])),
        "precision": ratio(len(relevant), len(briefed)),
        "hit_at_8": ratio(sum(r["hit"] for r in hit_pool), len(hit_pool)),
        "hit_pool": len(hit_pool),
    }


def replay_pass(rows, binary, replayer, warmed):
    results, unindexed = [], 0
    for row in rows:
        if not indexed(row["repo"]):
            unindexed += 1
            continue
        if row["repo"] not in warmed:
            warm(binary, row["repo"], replayer)
            warmed.add(row["repo"])
        brief = replayer(binary, row)
        named = named_files(brief, row["repo"]) if brief.strip() else []
        results.append({"id": row["id"], "label": row["label"], "source": row.get("source"),
                        "briefed": bool(brief.strip()), "bytes": len(brief), "touched": len(row.get("touched", [])),
                        "named": len(named), "hit": bool(set(named) & set(row.get("touched", [])))})
    return results, unindexed


def score(labels_path, binary, out_dir, name, replayer=replay, passes=3):
    """Replay every labelled row ``passes`` times and report the last pass. The gate's judge and the daemons warm up
    over the first prompts, so a cold first pass briefs fewer rows than the next ones (71 against 131 then 147 of 218
    on 2026-10-18; the third pass matched the second on every brief): the first pass is discarded when ``passes`` is
    above 2, and the rows whose verdict differs between the last two passes are reported as ``unstable``."""
    rows = [json.loads(line) for line in Path(labels_path).read_text(encoding="utf-8").splitlines() if line.strip()]
    bad = [r.get("id") for r in rows if r.get("label") not in LABELS]
    if bad:
        raise SystemExit(f"{len(bad)} rows have no label among {LABELS}: {bad[:5]}")
    warmed, runs = set(), []
    for _ in range(max(1, passes)):
        runs.append(replay_pass(rows, binary, replayer, warmed))
    results, unindexed = runs[-1]
    previous = runs[-2][0] if len(runs) > 1 else results
    unstable = sum(1 for a, b in zip(previous, results) if (a["briefed"], a["hit"]) != (b["briefed"], b["hit"]))
    real = Path(os.path.realpath(binary))
    report = {"label": name, "date": time.strftime("%Y-%m-%d"),
              "binary": {"path": str(binary), "sha256": file_sha256(real),
                         "version": subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip()},
              "labels": {"path": str(labels_path), "sha256": file_sha256(labels_path)},
              "window": [min((r["ts"] for r in rows if "ts" in r), default=None),
                         max((r["ts"] for r in rows if "ts" in r), default=None)],
              "command": " ".join(shlex.quote(a) for a in sys.argv), "unindexed": unindexed,
              "passes": max(1, passes), "unstable": unstable,
              "means": metrics(results),
              "by_source": {s: metrics([r for r in results if r["source"] == s])
                            for s in sorted({r["source"] for r in results})},
              "rows": results}
    out = Path(out_dir) / name
    out.mkdir(parents=True, exist_ok=True)
    (out / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    (out / "report.md").write_text(render(report), encoding="utf-8")
    print(render(report))
    return report


def pct(v):
    return "n/a" if v is None else f"{100 * v:.0f}%"


def render(report):
    m = report["means"]
    lines = [f"# Brief recall replay: {report['label']}", "",
             f"- binary: `{report['binary']['version'].splitlines()[0] if report['binary']['version'] else '?'}`, "
             f"sha256 `{report['binary']['sha256'][:12]}`",
             f"- labels: sha256 `{report['labels']['sha256'][:12]}`, {m['rows']} replayed rows, "
             f"{report['unindexed']} in unindexed repositories (left out)",
             f"- passes: {report.get('passes', 1)} (the last is reported); rows whose verdict changed between the last "
             f"two: {report.get('unstable', 0)}", "",
             "| group | rows | project | follow-up | other | briefed | project recall | follow-up recall | precision | "
             "hit@8 (pool) |", "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for name, g in [("all", m)] + list(report["by_source"].items()):
        lines.append(f"| {name} | {g['rows']} | {g['labels']['project']} | {g['labels']['follow-up']} | "
                     f"{g['labels']['other']} | {g['briefed']} | {pct(g['project_recall'])} | "
                     f"{pct(g['follow_up_recall'])} | {pct(g['precision'])} | {pct(g['hit_at_8'])} ({g['hit_pool']}) |")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

class SelfTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name) / "home"
        self.repo = Path(self.tmp.name) / "work" / "repo"
        (self.repo / "src").mkdir(parents=True)
        (self.repo / "src" / "gate.rs").write_text("fn gate() {}\n")
        (self.repo / "README.md").write_text("# repo\n")
        env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull)
        subprocess.run(["git", "init", "-q"], cwd=self.repo, check=True, env=env)
        self.t0 = parse_ts("2026-10-17T10:00:00Z")

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rel, lines):
        path = self.home / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(json.dumps(x) for x in lines) + "\n")

    def iso(self, minutes):
        return datetime.fromtimestamp(self.t0 + 60 * minutes, timezone.utc).isoformat().replace("+00:00", "Z")

    def test_extract_keeps_typed_prompts_with_their_touched_files_and_drops_harness_turns(self):
        cwd = str(self.repo)
        self.write(".claude/projects/p/a.jsonl", [
            {"type": "user", "cwd": cwd, "timestamp": self.iso(1), "entrypoint": "cli",
             "message": {"content": "where is the gate decided"}},
            {"type": "assistant", "cwd": cwd, "timestamp": self.iso(2), "entrypoint": "cli",
             "message": {"content": [{"type": "tool_use", "input": {"file_path": f"{cwd}/src/gate.rs"}}]}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(3), "entrypoint": "cli",
             "message": {"content": [{"type": "tool_result", "content": "fn gate"}]}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(4), "entrypoint": "cli",
             "message": {"content": "<command-name>/clear</command-name>"}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(4.2), "entrypoint": "cli",
             "message": {"content": "<bash-input>ls</bash-input><bash-stdout>a</bash-stdout>"}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(4.4), "entrypoint": "cli",
             "message": {"content": "This session is being continued from a previous conversation that ran out of context."}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(5), "entrypoint": "cli", "isSidechain": True,
             "message": {"content": "a subagent's task"}},
            {"type": "user", "cwd": cwd, "timestamp": self.iso(6), "entrypoint": "cli",
             "message": {"content": "and who calls it"}},
        ])
        self.write(".claude/projects/p/copy.jsonl", [   # a resumed session's copy of the first turn
            {"type": "user", "cwd": cwd, "timestamp": self.iso(1), "entrypoint": "cli",
             "message": {"content": "where is the gate decided [external unsupported block: image]"}}])
        self.write(".claude/projects/p/sdk.jsonl", [
            {"type": "user", "cwd": cwd, "timestamp": self.iso(1), "entrypoint": "sdk-cli",
             "message": {"content": "a program's prompt"}}])
        self.write(".codex/sessions/2026/10/17/r.jsonl", [
            {"type": "session_meta", "payload": {"originator": "Codex Desktop", "source": "vscode", "cwd": cwd}},
            {"type": "response_item", "timestamp": self.iso(7),
             "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the readme"}]}},
            {"type": "response_item", "timestamp": self.iso(8),
             "payload": {"type": "function_call", "arguments": json.dumps({"cmd": "sed -n 1,5p README.md"})}},
            {"type": "response_item", "timestamp": self.iso(8.5), "payload": {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "<external_codex_apps_open_page>{}</external_codex_apps_open_page>"}]}},
            {"type": "response_item", "timestamp": self.iso(8.7), "payload": {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "<in-app-browser-context source=\"x\">ui</in-app-browser-context>\n"
                                                         "and the gate tests"}]}},
        ])
        self.write(".codex/sessions/2026/10/17/e.jsonl", [
            {"type": "session_meta", "payload": {"originator": "codex_exec", "source": "exec", "cwd": cwd}},
            {"type": "response_item", "timestamp": self.iso(7),
             "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "bench"}]}}])
        self.write(".pi/agent/sessions/x/s.jsonl", [
            {"type": "session", "cwd": cwd},
            {"type": "message", "timestamp": self.iso(9), "message": {"role": "user", "content": "explain the gate"}},
            {"type": "message", "timestamp": self.iso(10),
             "message": {"role": "assistant", "content": [{"type": "toolCall", "arguments": {"path": "src/gate.rs"}}]}},
        ])
        rows = extract(self.t0, self.t0 + 3600, home=self.home, temp_roots=[str(Path(self.tmp.name) / "nowhere")])
        self.assertEqual([(r["source"], r["prompt"], r["touched"], r["follows"]) for r in rows], [
            ("claude", "where is the gate decided", ["src/gate.rs"], False),
            ("claude", "and who calls it", [], True),
            ("codex", "fix the readme", ["README.md"], False),
            ("codex", "and the gate tests", [], True),
            ("pi", "explain the gate", ["src/gate.rs"], False)])
        self.assertEqual({r["repo"] for r in rows}, {str(self.repo.resolve())})
        # A window that ends before the second prompt keeps only the first.
        self.assertEqual(len(extract(self.t0, self.t0 + 150, home=self.home,
                                     temp_roots=[str(Path(self.tmp.name) / "nowhere")])), 1)

    def test_score_counts_recall_precision_and_hit_at_8_and_leaves_unindexed_repos_apart(self):
        (self.repo / ".pixel").mkdir()
        (self.repo / ".pixel" / "base.shard").write_text("")
        other = Path(self.tmp.name) / "unindexed"
        other.mkdir()
        rows = [
            {"id": "a", "repo": str(self.repo), "prompt": "p1", "label": "project", "touched": ["src/gate.rs"]},
            {"id": "b", "repo": str(self.repo), "prompt": "p2", "label": "project", "touched": ["README.md"]},
            {"id": "c", "repo": str(self.repo), "prompt": "p3", "label": "project", "touched": []},
            {"id": "d", "repo": str(self.repo), "prompt": "p4", "label": "follow-up", "touched": []},
            {"id": "e", "repo": str(self.repo), "prompt": "p5", "label": "other", "touched": []},
            {"id": "f", "repo": str(other), "prompt": "p6", "label": "project", "touched": []},
        ]
        labels = Path(self.tmp.name) / "labels.jsonl"
        labels.write_text("\n".join(json.dumps(r) for r in rows) + "\n")
        briefs = {"p1": "### src/gate.rs:1-1 — gate\n1| fn gate() {}\n", "p2": "files: src/gate.rs:1\n",
                  "p5": "### README.md:1-1\n1| # repo\n"}
        fake = Path(self.tmp.name) / "fakepixel"
        fake.write_text("#!/bin/sh\necho fakepixel 0.0.1\n")
        fake.chmod(0o755)
        calls = []

        def replayer(_b, row):
            calls.append(row["prompt"])
            return briefs.get(row["prompt"], "")

        report = score(labels, str(fake), Path(self.tmp.name) / "out", "t", replayer=replayer, passes=1)
        # The indexed repository is warmed once, before its first scored prompt; the unindexed one never.
        self.assertEqual(calls, [WARM_PROMPT] * 3 + ["p1", "p2", "p3", "p4", "p5"])
        m = report["means"]
        self.assertEqual(report["unindexed"], 1)
        self.assertEqual((m["rows"], m["briefed"]), (5, 3))
        self.assertEqual(m["project_recall"], 2 / 3)        # p1, p2 briefed; p3 not
        self.assertEqual(m["follow_up_recall"], 0.0)
        self.assertEqual(m["precision"], 2 / 3)             # p5 is not a project prompt
        self.assertEqual((m["hit_at_8"], m["hit_pool"]), (0.5, 2))   # p1 names its touched file, p2 does not
        md = (Path(self.tmp.name) / "out" / "t" / "report.md").read_text()
        self.assertNotIn("p1", md.replace("hit@8", ""))     # no prompt text in the report
        self.assertNotIn('"prompt"', (Path(self.tmp.name) / "out" / "t" / "report.json").read_text())

    def test_score_reports_the_last_pass_and_counts_rows_that_changed_since_the_one_before(self):
        (self.repo / ".pixel").mkdir()
        (self.repo / ".pixel" / "base.shard").write_text("")
        rows = [{"id": k, "repo": str(self.repo), "prompt": k, "label": "project", "touched": []} for k in "abc"]
        labels = Path(self.tmp.name) / "labels.jsonl"
        labels.write_text("\n".join(json.dumps(r) for r in rows) + "\n")
        fake = Path(self.tmp.name) / "fakepixel"
        fake.write_text("#!/bin/sh\necho fakepixel 0.0.1\n")
        fake.chmod(0o755)
        seen = {}

        def warming(_b, row):   # a cold gate: a prompt is briefed from its second replay on, "c" from its third
            seen[row["prompt"]] = seen.get(row["prompt"], 0) + 1
            if row["prompt"] == WARM_PROMPT:
                return "w"
            return "### src/gate.rs:1-1\n" if seen[row["prompt"]] >= (3 if row["prompt"] == "c" else 2) else ""

        report = score(labels, str(fake), Path(self.tmp.name) / "out", "p", replayer=warming, passes=3)
        self.assertEqual((report["means"]["briefed"], report["passes"], report["unstable"]), (3, 3, 1))
        self.assertIn("rows whose verdict changed between the last two: 1",
                      (Path(self.tmp.name) / "out" / "p" / "report.md").read_text())

    def test_score_refuses_an_unlabelled_row(self):
        labels = Path(self.tmp.name) / "labels.jsonl"
        labels.write_text(json.dumps({"id": "a", "repo": str(self.repo), "prompt": "p"}) + "\n")
        with self.assertRaises(SystemExit):
            score(labels, "x", self.tmp.name, "t", replayer=lambda _b, _r: "")


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if "--self-test" in argv:
        argv.remove("--self-test")
        return 0 if unittest.main(argv=[sys.argv[0]] + argv, exit=False).result.wasSuccessful() else 1
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    ex = sub.add_parser("extract", help="human prompts of a window, with their repository and touched files")
    ex.add_argument("--since", required=True, help="ISO time (local when naive)")
    ex.add_argument("--until", default=None, help="ISO time (default: now)")
    ex.add_argument("--out", required=True)
    sc = sub.add_parser("score", help="replay labelled rows through the prompt-submit brief")
    sc.add_argument("--labels", required=True)
    sc.add_argument("--bin", required=True)
    sc.add_argument("--out", required=True)
    sc.add_argument("--label", default=None)
    sc.add_argument("--passes", type=int, default=3, help="replays of the whole set; the last one is reported")
    args = parser.parse_args(argv)
    if args.cmd == "extract":
        rows = extract(parse_ts(args.since), parse_ts(args.until) if args.until else time.time())
        Path(args.out).write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")
        print(f"{len(rows)} prompts written to {args.out}")
        return 0
    score(args.labels, args.bin, args.out, args.label or Path(args.bin).name, passes=args.passes)
    return 0


if __name__ == "__main__":
    sys.exit(main())
