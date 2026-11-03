#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Oracle experiment (issue #905): does an agent answer with zero tool calls when the prompt hook is perfect?

For each census session with ``mode=answer`` the "oracle" is the output of that
session's own exploration calls (reads, searches, listings, git reads), taken
from its raw stream. A fresh clone at the pinned commit gets a settings file
with ONE ``UserPromptSubmit`` hook that prints the oracle as ``additionalContext``.

    oracle.py cap-probe                       # the real size limit of additionalContext, with a marker
    oracle.py build                           # oracle briefs, hook payloads and settings
    oracle.py run --run-id oracle-1 [--arms P,D,PF,DF] [--only id,id] [--concurrency 12]
    oracle.py report --run-id oracle-1        # per-session metrics as Markdown rows
    oracle.py --self-test

Arms: ``P`` plain header, ``D`` header + directive footer, both through ONE hook
capped at 10 000 characters (the documented limit, verified by ``cap-probe``;
cited regions first when the oracle exceeds it). ``PF`` and ``DF`` carry the same
text UNtruncated through several hooks of at most 9 800 characters each (the
limit is per string, verified too); they exist only for sessions whose oracle
exceeds one hook, to separate "perfect context" from "delivery limit".

Reuses ``run.py`` (claude launch, env whitelist, shim, clone logic, quota guard)
and the call classifier of the extractor lane (``classify.py``). It works on the PRIVATE census only: the recorded
raw streams (``<root>/raw``), ``census.db`` and the local ``manifest.json`` of ``run.py``; ``--root`` (default
``$PIXEL_CENSUS_ROOT``) names the census root. The public bench has no such streams. Standard library only.
"""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import question_source as qs  # noqa: E402

ROOT = qs.default_root()
SCRATCH = ROOT / "oracle"
CLASSIFY_PATHS = (HERE / "classify.py",)

HOOK_CAP = 10_000      # documented per-string cap of additionalContext; confirmed by cap-probe
CHUNK = 9_800          # per-hook budget of the untruncated arms
HEADER = "Pixel already read these for you (exact outputs, current as of this commit):"
FOOTER = "Answer from this context. Use a tool only if something you need is not shown above."
ARMS = ("P", "D", "PF", "DF")
EXPLORATION = ("read", "search", "list", "git-read")


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


CENSUS = load("census_run", HERE / "run.py")


def classifier():
    for path in CLASSIFY_PATHS:
        if path.exists():
            return load("census_classify", path)
    raise SystemExit("classify.py (extractor lane, issue #905) not found")


# --------------------------------------------------------------------------
# Streams
# --------------------------------------------------------------------------

def flatten(content):
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return content if isinstance(content, str) else json.dumps(content)


def parse_stream(path):
    """Calls in order with their results and the assistant text said before each, plus final answer and hooks."""
    calls, by_id, texts, hooks = [], {}, [], []
    result = None
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        kind = rec.get("type")
        if kind == "assistant" and not rec.get("parent_tool_use_id"):
            for part in (rec.get("message") or {}).get("content") or []:
                if part.get("type") == "text" and part.get("text", "").strip():
                    texts.append(part["text"])
                elif part.get("type") == "tool_use" and part["id"] not in by_id:
                    call = {"id": part["id"], "tool": part["name"], "input": part.get("input") or {},
                            "result": None, "error": False, "said": "\n".join(texts)}
                    texts = []
                    by_id[part["id"]] = call
                    calls.append(call)
        elif kind == "user":
            content = (rec.get("message") or {}).get("content")
            for part in content if isinstance(content, list) else []:
                if isinstance(part, dict) and part.get("type") == "tool_result" and part.get("tool_use_id") in by_id:
                    by_id[part["tool_use_id"]]["result"] = flatten(part.get("content"))
                    by_id[part["tool_use_id"]]["error"] = bool(part.get("is_error"))
        elif kind == "system" and rec.get("subtype") in ("hook_started", "hook_response"):
            hooks.append({"subtype": rec["subtype"], "event": rec.get("hook_event"), "outcome": rec.get("outcome"),
                          "exit": rec.get("exit_code")})
        elif kind == "result":
            result = rec
    return {"calls": calls, "result": result, "hooks": hooks, "trailing_text": "\n".join(texts)}


def strip_ws(text, session_id):
    """Paths as the agent in a fresh clone sees them: relative to the repository root."""
    ws = re.escape(str(ROOT / "ws" / session_id))
    text = re.sub(r"cd " + ws + r"/?\s*(;|&&)\s*", "", text)
    return re.sub(ws + r"/?", "", text)


# --------------------------------------------------------------------------
# Oracle
# --------------------------------------------------------------------------

def is_exploration(cls, tool, inp):
    if cls in EXPLORATION:
        return True
    cmd = inp.get("command", "") if tool == "Bash" else ""
    # `git show` / `git log` chains are misfiled as git-write when a tag/ref lookup rides along; a pytest
    # --collect-only count is a listing of tests, not a run.
    return bool(re.search(r"\bgit (show|log|diff|blame|tag --contains)\b", cmd)) or "--collect-only" in cmd


def one_line(inp, session_id):
    inp = {k: v for k, v in inp.items() if k != "description"}
    if "command" in inp:
        return strip_ws(re.sub(r"\s+", " ", inp["command"]).strip(), session_id)[:300]
    parts = []
    for key, value in inp.items():
        parts.append(f"{key}={strip_ws(str(value), session_id)}")
    return " ".join(parts)[:300]


def read_range(inp):
    offset = int(inp.get("offset") or 1)
    limit = inp.get("limit")
    return offset, (offset + int(limit) - 1) if limit else None


def build_entries(session_id, calls, classify):
    """Exploration results in call order, de-duplicated (same call, same output, Read inside an earlier Read)."""
    entries, seen_text, reads = [], set(), []
    for call in calls:
        cls, _need = classify.classify(call["tool"], call["input"])
        if not is_exploration(cls, call["tool"], call["input"]) or call["result"] is None:
            continue
        text = strip_ws(call["result"], session_id).rstrip("\n")
        path = strip_ws(call["input"].get("file_path", ""), session_id) if call["tool"] == "Read" else None
        if call["tool"] == "Read":
            lo, hi = read_range(call["input"])
            if any(p == path and l <= lo and (h is None or (hi is not None and hi <= h)) for p, l, h in reads):
                continue
            reads.append((path, lo, hi))
        if text in seen_text:
            continue
        seen_text.add(text)
        label = f"{call['tool']} {one_line(call['input'], session_id)}"
        entries.append({"tool": call["tool"], "label": label, "text": text or "(no output)", "path": path,
                        "range": read_range(call["input"]) if path else None, "error": call["error"]})
    return entries


def fence(text):
    longest = max((len(m) for m in re.findall(r"`+", text)), default=0)
    ticks = "`" * max(3, longest + 1)
    return f"{ticks}\n{text}\n{ticks}"


def render(entry, text=None):
    return f"### {entry['label']}\n{fence(entry['text'] if text is None else text)}"


PATHISH = re.compile(r"[\w./-]+\.(?:rs|py|ts|tsx|js|mts|md|toml|json|yml|yaml|lock|cfg|txt)\b")


def paths_of(entry):
    found = set(PATHISH.findall(entry["text"])) | set(PATHISH.findall(entry["label"]))
    if entry["path"]:
        found.add(entry["path"])
    return found


def cited(entry, answer):
    return any(p in answer or p.rsplit("/", 1)[-1] in answer for p in paths_of(entry))


def cited_lines(answer):
    return {int(n) for n in re.findall(r"(?<![\w.])(\d{2,6})(?![\w.])", answer)}


def window(entry, answer, budget):
    """A Read result shortened to the lines around the line numbers the answer cites, then the head, within ``budget``."""
    lines = entry["text"].split("\n")
    cites = cited_lines(answer)
    numbered = []
    for index, line in enumerate(lines):
        match = re.match(r"\s*(\d+)\t", line)
        numbered.append(int(match.group(1)) if match else None)
    if not any(n is not None for n in numbered):
        return head_cut(entry["text"], budget)
    keep = set()
    for index, number in enumerate(numbered):
        if number is not None and any(abs(number - c) <= 12 for c in cites):
            keep.add(index)
    order = sorted(keep) + [i for i in range(len(lines)) if i not in keep]
    used, chosen = len(render(entry, "")) + 40, set()
    for index in order:
        cost = len(lines[index]) + 1
        if used + cost > budget:
            if index in keep:
                continue
            break
        used += cost
        chosen.add(index)
    out, gap = [], False
    for index, line in enumerate(lines):
        if index in chosen:
            out.append(line)
            gap = False
        elif not gap:
            out.append("[... lines not shown ...]")
            gap = True
    return "\n".join(out)


def head_cut(text, budget):
    room = max(0, budget - 120)
    return text[:room] + f"\n[... {len(text) - room} more characters not shown ...]" if len(text) > room else text


def fitted(entry, answer, room):
    """The most of ``entry`` that renders within ``room`` characters (cut around the lines the answer cites), or None."""
    budget = room - 4
    while budget >= 400:  # gap markers and the note cost characters of their own: shrink until it fits
        cut = window(entry, answer, budget) if entry["range"] else head_cut(entry["text"], budget)
        if len(render(entry, cut)) + 2 <= room:
            return cut
        budget -= 100
    return None


def assemble(entries, answer, directive, cap):
    """The hook text. Over ``cap``: first the lines each cited entry is cited for (at most 2 500 characters each, cited
    entries before the rest), then whatever room remains upgrades entries to whole, in the same order; kept in call order."""
    foot = ("\n\n" + FOOTER) if directive else ""
    frame = len(HEADER) + 2 + len(foot)
    sizes = [len(render(e)) + 2 for e in entries]
    plan = {i: None for i in range(len(entries))}
    if cap is not None and frame + sum(sizes) > cap:
        order = [i for i, e in enumerate(entries) if cited(e, answer)] + \
                [i for i, e in enumerate(entries) if not cited(e, answer)]
        used, plan = frame, {}
        for i in order:
            cut = fitted(entries[i], answer, min(cap - used, 2_500 + 120)) if sizes[i] > 2_500 + 120 else None
            if sizes[i] <= 2_500 + 120 and used + sizes[i] <= cap:
                plan[i], used = entries[i]["text"], used + sizes[i]
            elif cut is not None:
                plan[i], used = cut, used + len(render(entries[i], cut)) + 2
        for i in order:
            current = len(render(entries[i], plan[i])) + 2 if i in plan else 0
            if i in plan and plan[i] == entries[i]["text"]:
                continue
            cut = fitted(entries[i], answer, cap - used + current) if cap - used + current > 600 else None
            if cut is not None and (i not in plan or len(cut) > len(plan[i])):
                used += len(render(entries[i], cut)) + 2 - current
                plan[i] = cut
    blocks, report = [], []
    for i, e in enumerate(entries):
        if i in plan:
            text = e["text"] if plan[i] is None else plan[i]
            blocks.append(render(e, text))
            report.append({"label": e["label"], "chars": len(e["text"]), "kept": len(text),
                           "cited": cited(e, answer), "cut": text != e["text"]})
        else:
            report.append({"label": e["label"], "chars": len(e["text"]), "kept": 0, "cited": cited(e, answer),
                           "cut": True})
    text = HEADER + "\n\n" + "\n\n".join(blocks) + foot
    return text, report


def chunks(text, size=CHUNK):
    """Split at block boundaries (blank line before ###), then at lines; labels carry the order."""
    parts, current = [], ""
    for block in re.split(r"\n\n(?=### |" + re.escape(FOOTER) + ")", text):
        pieces = [block]
        if len(block) > size - 60:
            pieces, buf = [], ""
            for line in block.split("\n"):
                if len(buf) + len(line) + 1 > size - 60 and buf:
                    pieces.append(buf)
                    buf = ""
                buf += line + "\n"
            pieces.append(buf.rstrip("\n"))
        for piece in pieces:
            if current and len(current) + len(piece) + 2 > size - 60:
                parts.append(current)
                current = ""
            current += ("\n\n" if current else "") + piece
    if current:
        parts.append(current)
    total = len(parts)
    return [f"[context part {i}/{total}]\n{part}" for i, part in enumerate(parts, 1)]


# --------------------------------------------------------------------------
# Hooks and settings
# --------------------------------------------------------------------------

def write_hook(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit",
                                                       "additionalContext": text}}), encoding="utf-8")


def write_settings(path, hook_files):
    body = {"hooks": {"UserPromptSubmit": [{"hooks": [
        {"type": "command", "command": f"cat '{hook}'", "timeout": 10} for hook in hook_files]}]}}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(body) + "\n", encoding="utf-8")


def answer_sessions(db):
    return [r[0] for r in db.execute(
        "SELECT id FROM sessions WHERE source='census' AND mode='answer' AND ok=1 ORDER BY id")]


# --------------------------------------------------------------------------
# build
# --------------------------------------------------------------------------

def cmd_build(args):
    import sqlite3
    classify = classifier()
    db = sqlite3.connect(f"file:{ROOT / 'census.db'}?mode=ro", uri=True, timeout=60)
    rows = {r[0]: r for r in db.execute("SELECT id, final_answer FROM sessions WHERE source='census'")}
    summary = {}
    for sid in answer_sessions(db):
        stream = parse_stream(ROOT / "raw" / f"{sid}.stream.jsonl")
        answer = rows[sid][1] or ""
        entries = build_entries(sid, stream["calls"], classify)
        excluded = [f"{c['tool']} {one_line(c['input'], sid)}" for c in stream["calls"]
                    if not is_exploration(classify.classify(c["tool"], c["input"])[0], c["tool"], c["input"])]
        info = {"entries": len(entries), "excluded": excluded}
        for directive, arm, full_arm in ((False, "P", "PF"), (True, "D", "DF")):
            full, _ = assemble(entries, answer, directive, None)
            capped, report = assemble(entries, answer, directive, HOOK_CAP)
            write_hook(SCRATCH / "hooks" / f"{arm}-{sid}.json", capped)
            write_settings(SCRATCH / "settings" / f"{arm}-{sid}.json", [SCRATCH / "hooks" / f"{arm}-{sid}.json"])
            parts = chunks(full)
            files = []
            for index, part in enumerate(parts, 1):
                path = SCRATCH / "hooks" / f"{full_arm}-{sid}.{index}.json"
                write_hook(path, part)
                files.append(path)
            write_settings(SCRATCH / "settings" / f"{full_arm}-{sid}.json", files)
            info[arm] = {"chars": len(capped), "full_chars": len(full), "truncated": len(full) > HOOK_CAP,
                         "report": report, "chunks": [len(p) for p in parts]}
            (SCRATCH / "briefs").mkdir(parents=True, exist_ok=True)
            (SCRATCH / "briefs" / f"{arm}-{sid}.txt").write_text(capped, encoding="utf-8")
            (SCRATCH / "briefs" / f"{full_arm}-{sid}.txt").write_text(full, encoding="utf-8")
        summary[sid] = info
        p = info["P"]
        print(f"{sid:<13} entries {info['entries']:>2}  full {p['full_chars']:>6}  capped {p['chars']:>6}  "
              f"{'TRUNCATED' if p['truncated'] else 'fits'}  chunks {p['chunks']}  excluded {len(excluded)}")
    (SCRATCH / "build.json").write_text(json.dumps(summary, indent=1), encoding="utf-8")
    return 0


# --------------------------------------------------------------------------
# cap-probe
# --------------------------------------------------------------------------

def cmd_cap_probe(args):
    probe = SCRATCH / "probe2"
    probe.mkdir(parents=True, exist_ok=True)

    def payload(n, tag):
        """Exactly ``n`` characters of numbered lines, the last one ``<tag>END-ZEBRA``."""
        end = f"{tag}END-ZEBRA"
        lines, i = [], 0
        while sum(len(x) + 1 for x in lines) + len(end) + 21 < n:
            i += 1
            lines.append(f"{tag}{i:05d} abcdefghijklmnop")
        text = "\n".join(lines)
        pad = n - len(text) - 1 - len(end)
        return text + "\n" + "." * (pad - 1) + "\n" + end if pad > 1 else text + "\n" + end

    cases = {"9999": [9999], "10000": [10000], "10001": [10001], "12000": [12000], "two-hooks-9000": [9000, 9000]}
    asks = "Without tools: for each prefix A, B, C, D, E, F say if a line that is exactly <prefix>END-ZEBRA is visible. Reply one line."
    out = {}
    env = CENSUS.agent_env(os.environ, CENSUS.make_shim_dir(ROOT))
    for name, sizes in cases.items():
        files = []
        for index, size in enumerate(sizes):
            tag = "ABCDEF"[len(files)] if len(cases[name]) > 1 else {"9999": "A", "10000": "B", "10001": "C",
                                                                   "12000": "D"}[name]
            text = payload(size, tag)
            assert len(text) == size, (len(text), size)
            path = probe / f"{name}.{index}.json"
            write_hook(path, text)
            files.append(path)
        write_settings(probe / f"{name}.settings.json", files)
        argv = [str(CENSUS.CLAUDE), "-p", "--output-format", "stream-json", "--verbose", "--include-hook-events",
                "--setting-sources", "project,local", "--settings", str(probe / f"{name}.settings.json"),
                "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence", "--tools", "",
                "--model", "haiku", "--max-budget-usd", "0.5"]
        lines, _, code, _, _ = CENSUS.run_claude(argv, asks, probe, env, 300)
        result = next((json.loads(x[1]) for x in lines if '"type":"result"' in x[1].replace(" ", "")), {})
        out[name] = result.get("result")
        print(f"{name:>16}: {out[name]}")
    (probe / "cap-probe.json").write_text(json.dumps(out, indent=1), encoding="utf-8")
    return 0


# --------------------------------------------------------------------------
# run
# --------------------------------------------------------------------------

def session_metrics(arm, sid, stream, lines, code, timed_out, n_hooks, classify):
    base = CENSUS.summarize(lines, code, timed_out)
    problems = [p for p in base["problems"] if not p.startswith("hooks:")]
    events = stream["hooks"]
    if not events or any(e["event"] != "UserPromptSubmit" for e in events) or len(events) != 2 * n_hooks or \
            any(e["outcome"] not in (None, "success") for e in events if e["subtype"] == "hook_response"):
        problems.append(f"hook-events:{len(events)}")
    calls = []
    for call in stream["calls"]:
        cls, need = classify.classify(call["tool"], call["input"])
        calls.append({"tool": call["tool"], "class": cls, "need": need,
                      "args": one_line(call["input"], sid), "said": call["said"][:600],
                      "exploration": is_exploration(cls, call["tool"], call["input"]),
                      "result_chars": len(call["result"] or "")})
    return {"arm": arm, "session": sid, "problems": problems, "calls": calls, "n_calls": len(calls),
            "n_exploration": sum(1 for c in calls if c["exploration"]),
            "first": calls[0] if calls else None, "tokens_in": base["tokens_in"], "tokens_out": base["tokens_out"],
            "cost_usd": base["cost_usd"], "num_turns": base["num_turns"], "answer": base["answer"],
            "rl_five_hour": base["rl_five_hour"]}


class Campaign:
    def __init__(self, args, classify, shim):
        self.args, self.classify, self.shim = args, classify, shim
        self.stop, self.reason, self.lock, self.records, self.done = threading.Event(), None, threading.Lock(), [], 0

    def halt(self, reason):
        if not self.stop.is_set():
            self.stop.set()
            self.reason = reason
            print(f"STOP: {reason}", flush=True)

    def on_rate(self, rate, proc):
        util = CENSUS.five_hour(rate)
        if util is not None and util >= self.args.max_utilization:
            self.halt(f"five-hour utilisation {util} >= {self.args.max_utilization}: no further launch")
        if util is not None and util >= self.args.kill_utilization:
            self.halt(f"five-hour utilisation {util} >= {self.args.kill_utilization}: killing")
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except OSError:
                pass

    def unit(self, arm, sid, prompt, sha, repo, total):
        if self.stop.is_set():
            return None
        ws_id = f"or-{arm}-{sid}"
        ws = CENSUS.make_workspace(ROOT, repo, ws_id, sha)
        settings = SCRATCH / "settings" / f"{arm}-{sid}.json"
        n_hooks = len(json.loads(settings.read_text())["hooks"]["UserPromptSubmit"][0]["hooks"])
        argv = CENSUS.claude_argv(CENSUS.CLAUDE, settings, self.args.model, self.args.effort, self.args.budget)
        lines, err, code, timed_out, seconds = CENSUS.run_claude(
            argv, prompt, ws, CENSUS.agent_env(os.environ, self.shim), self.args.timeout, self.on_rate)
        raw = SCRATCH / "raw" / f"{self.args.run_id}.{arm}-{sid}.stream.jsonl"
        raw.parent.mkdir(parents=True, exist_ok=True)
        raw.write_text("".join(x if x.endswith("\n") else x + "\n" for _, x in lines), encoding="utf-8")
        stream = parse_stream(raw)
        row = session_metrics(arm, sid, stream, lines, code, timed_out, n_hooks, self.classify)
        dirty = CENSUS.sh(["git", "status", "--porcelain"], cwd=ws, check=False).stdout.strip()
        left = CENSUS.sh(["git", "merge-base", "--is-ancestor", sha, "HEAD"], cwd=ws, check=False).returncode != 0
        if dirty:
            row["problems"].append("answer-mode-dirty")
        if left:
            row["problems"].append("left-pin")
        row.update({"wall_s": round(seconds, 1), "dirty": bool(dirty), "stderr_tail": err[-200:], "raw": str(raw)})
        shutil.rmtree(ws, ignore_errors=True)
        with self.lock:
            self.records.append(row)
            self.done += 1
            print(f"[{self.done}/{total}] {arm:<2} {sid:<13} calls {row['n_calls']:>2} (expl {row['n_exploration']:>2}) "
                  f"{seconds:>5.1f}s ${row['cost_usd'] or 0:.3f} 5h={row['rl_five_hour']} {','.join(row['problems'])}",
                  flush=True)
        if row["rl_five_hour"] is not None and row["rl_five_hour"] >= self.args.max_utilization:
            self.halt(f"five-hour utilisation {row['rl_five_hour']} >= {self.args.max_utilization}")
        return row


def cmd_run(args):
    classify = classifier()
    manifest = {r["id"]: r for r in CENSUS.load_manifest()}
    build = json.loads((SCRATCH / "build.json").read_text())
    arms = args.arms.split(",")
    only = set(args.only.split(",")) if args.only else None
    jobs = []
    for sid, info in build.items():
        if only and sid not in only:
            continue
        for arm in arms:
            if arm in ("PF", "DF") and not info["P"]["truncated"]:
                continue  # identical to P / D for a session whose oracle fits one hook
            jobs.append((arm, sid))
    pins = CENSUS.prepare_cache(ROOT)
    shim = CENSUS.make_shim_dir(ROOT)
    before = CENSUS.quota(ROOT, shim)
    print(f"five-hour utilisation before: {before}", flush=True)
    if before is None or before > args.start_ceiling:
        raise SystemExit(f"five-hour utilisation {before}: refusing to start (ceiling {args.start_ceiling})")
    camp = Campaign(args, classify, shim)
    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = [pool.submit(camp.unit, arm, sid, manifest[sid]["prompt"], manifest[sid]["sha"],
                               manifest[sid]["repo"], len(jobs)) for arm, sid in jobs]
        for future in concurrent.futures.as_completed(futures):
            future.result()
    after = CENSUS.quota(ROOT, shim)
    record = {"run_id": args.run_id, "five_hour_before": before, "five_hour_after": after,
              "wall_s": round(time.monotonic() - t0, 1), "stop_reason": camp.reason, "pins": pins,
              "cost_usd": round(sum(r["cost_usd"] or 0 for r in camp.records), 4),
              "claude_sha256": CENSUS.sha256_file(CENSUS.CLAUDE), "argv": CENSUS.claude_argv(
                  "<claude>", "<settings>", args.model, args.effort, args.budget),
              "sessions": sorted(camp.records, key=lambda r: (r["session"], r["arm"]))}
    out = SCRATCH / "runs" / f"{args.run_id}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(record, indent=1), encoding="utf-8")
    print(f"record {out}\ncost ${record['cost_usd']:.3f}  5h {before} -> {after}  wall {record['wall_s']}s")
    return 0


# --------------------------------------------------------------------------
# report
# --------------------------------------------------------------------------

def cmd_report(args):
    import sqlite3
    db = sqlite3.connect(f"file:{ROOT / 'census.db'}?mode=ro", uri=True, timeout=60)
    census = {r[0]: r[1] for r in db.execute(
        "SELECT session, count(*) FROM calls WHERE session IN (SELECT id FROM sessions WHERE source='census') "
        "GROUP BY session")}
    sessions = {}
    for run_id in args.run_id.split(","):
        record = json.loads((SCRATCH / "runs" / f"{run_id}.json").read_text())
        for row in record["sessions"]:
            sessions.setdefault(row["session"], {})[row["arm"]] = row
    for sid, arms in sorted(sessions.items()):
        cells = []
        for arm in ARMS:
            row = arms.get(arm)
            cells.append("-" if not row else f"{row['n_calls']} ({row['n_exploration']})")
        print(f"| {sid} | {census.get(sid, 0)} | " + " | ".join(cells) + " |")
    return 0


# --------------------------------------------------------------------------
# self-test
# --------------------------------------------------------------------------

class SelfTest(unittest.TestCase):
    def entry(self, n, label="Read a.rs"):
        return {"tool": "Read", "label": label, "text": "\n".join(f"{i}\tline {i}" for i in range(1, n + 1)),
                "path": "a.rs", "range": (1, n), "error": False}

    def test_small_oracle_is_untouched_and_framed(self):
        text, report = assemble([self.entry(3)], "", True, HOOK_CAP)
        self.assertTrue(text.startswith(HEADER) and text.endswith(FOOTER))
        self.assertFalse(report[0]["cut"])

    def test_cap_is_respected_and_cited_entry_survives(self):
        a, b = self.entry(900, "Read a.rs"), self.entry(900, "Read b.rs")
        b["path"] = "b.rs"
        text, report = assemble([a, b], "see b.rs line 450", False, HOOK_CAP)
        self.assertLessEqual(len(text), HOOK_CAP)
        self.assertTrue(report[1]["cited"] and report[1]["kept"] > 0)
        self.assertIn("450\tline 450", text)

    def test_chunks_cover_everything_within_the_limit(self):
        text, _ = assemble([self.entry(1500), self.entry(1500, "Read c.rs")], "", True, None)
        parts = chunks(text)
        self.assertGreater(len(parts), 1)
        self.assertTrue(all(len(p) <= CHUNK for p in parts))
        joined = "\n".join(parts)
        for needle in ("1500\tline 1500", HEADER, FOOTER):
            self.assertIn(needle, joined)

    def test_strip_ws_makes_paths_relative(self):
        self.assertEqual(strip_ws(f"{ROOT}/ws/x-1/src/a.py:3", "x-1"), "src/a.py:3")


def main():
    if "--self-test" in sys.argv:
        sys.argv.remove("--self-test")
        unittest.main(argv=[sys.argv[0]])
        return 0
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--root", default=None, help="census root (default: $PIXEL_CENSUS_ROOT, else $TMPDIR/pixel-tool-census)")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("cap-probe")
    sub.add_parser("build")
    run = sub.add_parser("run")
    run.add_argument("--run-id", required=True)
    run.add_argument("--arms", default="P,D,PF,DF")
    run.add_argument("--only")
    run.add_argument("--concurrency", type=int, default=12)
    run.add_argument("--model", default=None)
    run.add_argument("--effort", default=None)
    run.add_argument("--budget", type=float, default=2.0)
    run.add_argument("--timeout", type=int, default=900)
    run.add_argument("--max-utilization", type=float, default=0.9)
    run.add_argument("--kill-utilization", type=float, default=0.97)
    run.add_argument("--start-ceiling", type=float, default=0.75)
    report = sub.add_parser("report")
    report.add_argument("--run-id", required=True)
    args = parser.parse_args()
    if args.root:
        global ROOT, SCRATCH
        ROOT, SCRATCH = Path(args.root), Path(args.root) / "oracle"
    return {"cap-probe": cmd_cap_probe, "build": cmd_build, "run": cmd_run, "report": cmd_report}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
