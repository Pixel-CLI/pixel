#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Measure the prompt-submit brief on a labelled prompt set (issue #883).

For every row of ``eval/brief-gate/prompts.jsonl`` this runs ``pixel brief
<prompt> <repo>`` (the command the prompt-submit hook goes through), and
records whether a brief came back (the gate fired), the paths the brief
names, and the wall time of the call. It reports

* gate precision / recall / F1 against the ``on_topic`` label, overall and by
  language, source and (for off-topic rows) kind, plus the false-positive
  rate on the ops and chat rows;
* file recall@8 and hit@8 on the rows that carry ``expected_files`` (the first
  eight paths of the brief in rendered order, and the ``files:`` line alone);
* p50 / p95 latency.

The same command measures any binary against the same set, so a before and
an after differ only in ``--pixel``::

    python3 scripts/bench-brief-gate.py --pixel target/dev-release/pixel \\
        --repo <indexed checkout at the fixture SHA> --split all --out result.json

``--check-set`` validates the set (schema, duplicate ids, split balance, a
privacy lint, and with ``--repo`` that every expected file exists there);
``--self-test`` runs the parser and metric tests. Standard library only.
"""

import argparse
import hashlib
import json
import math
import os
import platform
import re
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import unittest
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

# The checkout every expected_files path was read against. A different repo
# HEAD still runs, but the result records ``fixture_match: false``: the
# labels belong to this tree, not to a later one.
FIXTURE_SHA = "85bede7d9a3c4e385e0a2045241a6466efbd8cbd"

DEFAULT_SET = Path(__file__).resolve().parent.parent / "eval" / "brief-gate" / "prompts.jsonl"
BRIEF_TAG = "[PIXEL:BRIEF]"
FILES_AT = 8  # the brief renders at most this many `files:` entries
SOURCES = ("session-context", "recall", "intent-eval", "synthetic", "real-paraphrased")
KINDS_ON = ("plain", "identifier")
KINDS_OFF = ("ops", "chat", "paste", "generic-code", "other-repo", "meta")
OPS_CHAT = ("ops", "chat")
LANGS = ("en", "fr")
SPLITS = ("dev", "test")
OLLAYA_ADDR = ("127.0.0.1", 11435)  # decide_ollaya::DEFAULT_BASE, the weak-signal judge
CANARY_PROMPT = "where is code_signal defined"  # names a code token: any working brief fires on it

# --------------------------------------------------------------------------
# Brief parsing
# --------------------------------------------------------------------------

PATH_ENTRY = re.compile(r"^(?P<path>[A-Za-z0-9_@+.\-]+(?:/[A-Za-z0-9_@+.\-]+)*)(?::(?P<line>\d+))?$")
MORE_TAIL = re.compile(r"\s*\(\+\d+ more\)\s*$")


def looks_like_path(token):
    """A repo-relative file path: a slash or an extension, no spaces."""
    match = PATH_ENTRY.match(token)
    if not match:
        return None
    path = match.group("path")
    name = path.rsplit("/", 1)[-1]
    if "/" not in path and "." not in name.strip("."):
        return None
    return path


def parse_files_line(value):
    """Paths of a `files:` value, in rendered order.

    The renderer joins entries with ``"; "`` when any carries a text
    (``path:line — text``) and with a space when none does. A text may hold
    ``"; "`` itself, so a segment only counts when it starts with a path.
    """
    value = MORE_TAIL.sub("", value.strip())
    if value in ("", "none"):
        return []
    if " — " in value:
        found = []
        for segment in value.split("; "):
            head = segment.split(" — ", 1)[0].strip()
            path = looks_like_path(head)
            if path and path not in found:
                found.append(path)
        return found
    found = []
    for token in value.split():
        path = looks_like_path(token)
        if path and path not in found:
            found.append(path)
    return found


def parse_defined_line(value):
    """Paths of a `defined:` value: ``kind name path:start-end — head``."""
    found = []
    for segment in MORE_TAIL.sub("", value).split("; "):
        head = segment.split(" — ", 1)[0].split()
        if len(head) < 3:
            continue
        site = head[-1].rsplit(":", 1)[0] if re.search(r":\d+(-\d+)?$", head[-1]) else head[-1]
        path = looks_like_path(site)
        if path and path not in found:
            found.append(path)
    return found


def parse_brief(text):
    """Structure of a ``[PIXEL:BRIEF]`` block; ``fired`` is False for anything else."""
    text = text or ""
    block = {"fired": False, "bytes": len(text.encode("utf-8")), "kind": None, "anchors": [],
             "files": [], "defined": [], "tests": [], "targets": [], "callers": [],
             "answered": None, "ops": None, "partial": False}
    if not text.lstrip().startswith(BRIEF_TAG):
        return block
    block["fired"] = True
    for line in text.splitlines():
        key, sep, value = line.partition(": ")
        if not sep:
            continue
        if key == "kind":
            block["kind"] = value.strip()
        elif key == "anchors":
            block["anchors"] = [item.strip() for item in value.split(",") if item.strip()]
        elif key == "files":
            block["files"] = parse_files_line(value)
        elif key == "defined":
            block["defined"] = parse_defined_line(value)
        elif key in ("tests", "targets"):
            block[key] = [p for p in map(looks_like_path, MORE_TAIL.sub("", value).split()) if p]
        elif key.startswith("callers"):
            block["callers"] = [p for p in (looks_like_path(seg.split(" -> ")[0].strip())
                                            for seg in MORE_TAIL.sub("", value).split("; ")) if p]
        elif key == "coverage":
            match = re.match(r"(\d+)/(\d+) ops answered(.*)", value)
            if match:
                block["answered"], block["ops"] = int(match.group(1)), int(match.group(2))
                block["partial"] = "partial" in match.group(3)
    if "packet partial" in text:
        block["partial"] = True
    return block


def mentioned_paths(block):
    """Every path the brief names, any section, first mention first."""
    seen = []
    for name in ("defined", "files", "callers", "tests", "targets"):
        for path in block[name]:
            if path not in seen:
                seen.append(path)
    return seen


# --------------------------------------------------------------------------
# Set loading and validation
# --------------------------------------------------------------------------

REQUIRED = ("id", "text", "lang", "on_topic", "source", "kind", "split")
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}")
IPV4 = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")
TOKENISH = re.compile(r"\b(?:sk|pk|ghp|gho|ghs|xox[abp]|glpat|AKIA|AIza)[-_A-Za-z0-9]{10,}")
LONG_SECRET = re.compile(r"\b[A-Za-z0-9+/_=-]{40,}\b")
URL = re.compile(r"https?://[^\s)>\"']+")
ALLOWED_URL_PREFIXES = ("https://github.com/Pixel-CLI/pixel",)


def privacy_problems(label, value):
    """Public-repo hygiene for one string of the set."""
    problems = []
    if EMAIL.search(value):
        problems.append(f"{label}: contains an email address")
    if "/Users/" in value or "/home/" in value:
        problems.append(f"{label}: contains a local home path")
    if IPV4.search(value):
        problems.append(f"{label}: contains an IPv4 address")
    if TOKENISH.search(value) or "-----BEGIN" in value:
        problems.append(f"{label}: looks like a credential")
    for word in LONG_SECRET.findall(value):
        if "/" not in word and not word.isalpha():
            problems.append(f"{label}: contains a long token-like string")
            break
    for url in URL.findall(value):
        if not url.startswith(ALLOWED_URL_PREFIXES):
            problems.append(f"{label}: contains a URL outside the project ({url[:40]})")
    return problems


def load_set(path):
    rows = []
    with open(path, encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            if not line.strip():
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise SystemExit(f"{path}:{number}: not JSON ({error})")
    return rows


def validate_rows(rows, repo_files=None):
    """Every problem found in a set; an empty list means it is usable."""
    problems = []
    ids = Counter(row.get("id") for row in rows)
    texts = Counter(row.get("text") for row in rows)
    for row in rows:
        rid = row.get("id", "<no id>")
        for field in REQUIRED:
            if field not in row:
                problems.append(f"{rid}: missing `{field}`")
        if ids[row.get("id")] > 1:
            problems.append(f"{rid}: duplicate id")
        if texts[row.get("text")] > 1:
            problems.append(f"{rid}: duplicate text")
        if not isinstance(row.get("text"), str) or not row.get("text", "").strip():
            problems.append(f"{rid}: empty text")
        if row.get("lang") not in LANGS:
            problems.append(f"{rid}: lang must be one of {LANGS}")
        if row.get("source") not in SOURCES:
            problems.append(f"{rid}: source must be one of {SOURCES}")
        if row.get("split") not in SPLITS:
            problems.append(f"{rid}: split must be one of {SPLITS}")
        if not isinstance(row.get("on_topic"), bool):
            problems.append(f"{rid}: on_topic must be a boolean")
        allowed = KINDS_ON if row.get("on_topic") else KINDS_OFF
        if row.get("kind") not in allowed:
            problems.append(f"{rid}: kind {row.get('kind')!r} not valid for on_topic={row.get('on_topic')}")
        expected = row.get("expected_files")
        if expected is not None:
            if not row.get("on_topic"):
                problems.append(f"{rid}: expected_files on an off-topic row")
            if not (isinstance(expected, list) and expected and all(isinstance(p, str) for p in expected)):
                problems.append(f"{rid}: expected_files must be a non-empty list of strings")
            else:
                for path in expected:
                    if path.startswith("/") or ".." in path.split("/"):
                        problems.append(f"{rid}: expected file {path} is not repo-relative")
                    elif repo_files is not None and path not in repo_files:
                        problems.append(f"{rid}: expected file {path} does not exist in the repo")
        for label in ("text", "evidence"):
            if isinstance(row.get(label), str):
                problems.extend(f"{rid}: {p}" for p in privacy_problems(label, row[label]))
    strata = defaultdict(Counter)
    for row in rows:
        strata[(row.get("on_topic"), row.get("lang"))][row.get("split")] += 1
    for key, counts in sorted(strata.items(), key=str):
        if abs(counts["dev"] - counts["test"]) > 1:
            problems.append(f"split unbalanced for on_topic={key[0]} lang={key[1]}: {dict(counts)}")
    return problems


def summarize_set(rows):
    count = Counter
    return {
        "rows": len(rows),
        "on_topic": sum(1 for r in rows if r["on_topic"]),
        "off_topic": sum(1 for r in rows if not r["on_topic"]),
        "lang": dict(count(r["lang"] for r in rows)),
        "split": dict(count(r["split"] for r in rows)),
        "source": dict(count(r["source"] for r in rows)),
        "kind": dict(count(r["kind"] for r in rows)),
        "with_expected_files": sum(1 for r in rows if r.get("expected_files")),
    }


# --------------------------------------------------------------------------
# Metrics
# --------------------------------------------------------------------------

def ratio(numerator, denominator):
    return None if denominator == 0 else numerator / denominator


def confusion(pairs):
    """Gate counts for ``(on_topic, fired)`` pairs, with precision, recall and F1."""
    tp = sum(1 for on, fired in pairs if on and fired)
    fp = sum(1 for on, fired in pairs if not on and fired)
    fn = sum(1 for on, fired in pairs if on and not fired)
    tn = sum(1 for on, fired in pairs if not on and not fired)
    precision, recall = ratio(tp, tp + fp), ratio(tp, tp + fn)
    f1 = None if precision is None or recall is None or precision + recall == 0 \
        else 2 * precision * recall / (precision + recall)
    return {"n": len(pairs), "tp": tp, "fp": fp, "fn": fn, "tn": tn,
            "precision": precision, "recall": recall, "f1": f1}


def percentile(values, q):
    """Nearest-rank percentile; ``None`` for no values."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(q / 100 * len(ordered)))
    return ordered[rank - 1]


def latency_stats(values):
    return {"n": len(values), "p50": percentile(values, 50), "p95": percentile(values, 95),
            "max": max(values) if values else None}


def file_scores(expected, block):
    """How many expected files the brief names.

    ``recall``/``hit`` use the first eight paths of the brief in the order it
    renders them (``defined``, ``files``, ``callers``, ``tests``, ``targets``),
    what an agent reading the block top-down sees first. ``files_line_*`` use
    the ``files:`` line alone, and ``any_hit`` every path the brief names.
    """
    wanted = list(dict.fromkeys(expected))
    named = mentioned_paths(block)
    top, files_line = named[:FILES_AT], block["files"][:FILES_AT]
    hits = [p for p in wanted if p in top]
    line_hits = [p for p in wanted if p in files_line]
    return {"recall": len(hits) / len(wanted), "hit": bool(hits),
            "files_line_recall": len(line_hits) / len(wanted), "files_line_hit": bool(line_hits),
            "any_hit": any(p in named for p in wanted)}


def evaluate(rows, outcomes):
    """Metrics for ``rows``; ``outcomes`` maps id -> {fired, block, latency_ms}."""
    scored = [(row, outcomes[row["id"]]) for row in rows if row["id"] in outcomes]
    pairs = [(row["on_topic"], out["fired"]) for row, out in scored]
    metrics = {"gate": {"overall": confusion(pairs)}}
    for field in ("lang", "source"):
        metrics["gate"][f"by_{field}"] = {
            value: confusion([(r["on_topic"], o["fired"]) for r, o in scored if r[field] == value])
            for value in sorted({r[field] for r, _ in scored})}
    on_rows = [(r, o) for r, o in scored if r["on_topic"]]
    metrics["gate"]["on_topic_recall_by_kind"] = {
        kind: {"n": len(sel), "fired": sum(o["fired"] for _, o in sel),
               "recall": ratio(sum(o["fired"] for _, o in sel), len(sel))}
        for kind in sorted({r["kind"] for r, _ in on_rows})
        for sel in [[(r, o) for r, o in on_rows if r["kind"] == kind]]}
    off_rows = [(r, o) for r, o in scored if not r["on_topic"]]
    metrics["gate"]["off_topic_fp_by_kind"] = {
        kind: {"n": len(sel), "fired": sum(o["fired"] for _, o in sel),
               "fp_rate": ratio(sum(o["fired"] for _, o in sel), len(sel))}
        for kind in sorted({r["kind"] for r, _ in off_rows})
        for sel in [[(r, o) for r, o in off_rows if r["kind"] == kind]]}
    oc = [(r, o) for r, o in off_rows if r["kind"] in OPS_CHAT]
    metrics["gate"]["ops_chat_fp"] = {"n": len(oc), "fired": sum(o["fired"] for _, o in oc),
                                      "fp_rate": ratio(sum(o["fired"] for _, o in oc), len(oc))}
    metrics["gate"]["off_topic_fp"] = {"n": len(off_rows), "fired": sum(o["fired"] for _, o in off_rows),
                                       "fp_rate": ratio(sum(o["fired"] for _, o in off_rows), len(off_rows))}
    with_files = [(r, o) for r, o in scored if r.get("expected_files")]
    per_row = [file_scores(r["expected_files"], o["block"]) for r, o in with_files]
    fired_idx = [i for i, (_, o) in enumerate(with_files) if o["fired"]]
    mean = lambda items: None if not items else sum(items) / len(items)  # noqa: E731
    metrics["files"] = {
        "rows": len(with_files),
        "fired_rows": len(fired_idx),
        f"recall@{FILES_AT}": mean([s["recall"] for s in per_row]),
        f"hit@{FILES_AT}": mean([s["hit"] for s in per_row]),
        f"recall@{FILES_AT}_when_fired": mean([per_row[i]["recall"] for i in fired_idx]),
        f"hit@{FILES_AT}_when_fired": mean([per_row[i]["hit"] for i in fired_idx]),
        f"files_line_recall@{FILES_AT}": mean([s["files_line_recall"] for s in per_row]),
        f"files_line_hit@{FILES_AT}": mean([s["files_line_hit"] for s in per_row]),
        "any_section_hit": mean([s["any_hit"] for s in per_row]),
    }
    metrics["files"]["by_kind"] = {
        kind: {"rows": len(idx), "fired": sum(1 for i in idx if with_files[i][1]["fired"]),
               f"hit@{FILES_AT}": mean([per_row[i]["hit"] for i in idx]),
               f"hit@{FILES_AT}_when_fired": mean([per_row[i]["hit"] for i in idx if with_files[i][1]["fired"]])}
        for kind in sorted({r["kind"] for r, _ in with_files})
        for idx in [[i for i, (r, _) in enumerate(with_files) if r["kind"] == kind]]}
    metrics["latency_ms"] = {
        "all": latency_stats([o["latency_ms"] for _, o in scored]),
        "fired": latency_stats([o["latency_ms"] for _, o in scored if o["fired"]]),
    }
    return metrics


# --------------------------------------------------------------------------
# Running pixel
# --------------------------------------------------------------------------

def brief_env():
    """The environment of one brief call: the brief forced on (it ignores a
    ``brief: false`` in any pixel config), the weak-signal judge's debug line kept."""
    env = dict(os.environ)
    env["PIXEL_BRIEF"] = "1"
    env["PIXEL_BRIEF_DEBUG"] = "1"
    env["NO_COLOR"] = "1"
    return env


def run_brief(pixel, repo, prompt, timeout):
    started = time.perf_counter()
    try:
        done = subprocess.run([str(pixel), "brief", "--metrics", "off", prompt, str(repo)],
                              cwd=repo, capture_output=True, text=True, timeout=timeout, env=brief_env())
        code, out, err = done.returncode, done.stdout, done.stderr
    except subprocess.TimeoutExpired:
        code, out, err = -9, "", "timeout"
    return code, out, err, (time.perf_counter() - started) * 1000


def check_canary(pixel, repo, timeout):
    """Whether a prompt that names a code token gets a brief; ``(ok, why)``.

    An index that does not cover HEAD, or a binary that cannot read it,
    returns nothing for every prompt, and a bench that went on would report a
    gate that never fires.
    """
    code, out, err, _ = run_brief(pixel, repo, CANARY_PROMPT, timeout)
    if parse_brief(out)["fired"]:
        return True, ""
    return False, f"exit {code}, stdout {out[:80]!r}, stderr {err.strip()[:160]!r}"


def judge_note(stderr):
    """What the weak-signal intent judge reported on stderr, if it ran."""
    for line in stderr.splitlines():
        if line.startswith("pixel-brief intent:"):
            return line.split(":", 1)[1].strip()
    return None


def measure_row(pixel, repo, row, repeat, timeout):
    runs = [run_brief(pixel, repo, row["text"], timeout) for _ in range(repeat)]
    code, out, err, _ = runs[0]
    block = parse_brief(out)
    return {"id": row["id"], "fired": block["fired"], "rc": code, "block": block,
            "latency_ms": round(statistics.median(r[3] for r in runs), 2),
            "stable": len({r[1] for r in runs}) == 1, "judge": judge_note(err),
            "stdout_sha": hashlib.sha256(out.encode()).hexdigest()[:12]}


def daemon_running(pixel, repo):
    done = subprocess.run([str(pixel), "daemon", "status", "--metrics", "off", str(repo)],
                          capture_output=True, text=True, timeout=30)
    text = done.stdout + done.stderr
    return "daemon running" in text and "not running" not in text


def set_daemon(pixel, repo, mode):
    """Bring the repo's daemon to ``mode`` and return whether it is running."""
    verb = "start" if mode == "on" else "stop"
    subprocess.run([str(pixel), "daemon", verb, "--metrics", "off", str(repo)],
                   capture_output=True, text=True, timeout=60)
    deadline = time.time() + 15
    while time.time() < deadline:
        if daemon_running(pixel, repo) == (mode == "on"):
            return mode == "on"
        time.sleep(0.2)
    return daemon_running(pixel, repo)


def ollaya_warm():
    try:
        with socket.create_connection(OLLAYA_ADDR, timeout=0.3):
            return True
    except OSError:
        return False


def git_output(repo, *args):
    done = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    return done.stdout.strip() if done.returncode == 0 else None


def tracked_files(repo):
    listing = git_output(repo, "ls-files")
    return set(listing.splitlines()) if listing else set()


def repo_is_indexed(repo):
    return (Path(repo) / ".pixel" / "base.shard").is_file()


def version_of(pixel):
    done = subprocess.run([str(pixel), "--version"], capture_output=True, text=True, timeout=30)
    return [line.strip() for line in done.stdout.splitlines() if line.strip()][:2]


# --------------------------------------------------------------------------
# Report
# --------------------------------------------------------------------------

def pct(value):
    return "n/a" if value is None else f"{value * 100:.1f}%"


def ms(value):
    return "n/a" if value is None else f"{value:.0f}"


def render_markdown(result):
    gate, files, lat = result["metrics"]["gate"], result["metrics"]["files"], result["metrics"]["latency_ms"]
    out = []
    meta = result["meta"]
    out.append(f"split `{meta['split']}`, {result['set']['rows_run']} prompts, daemon `{meta['daemon']}`, "
               f"repo `{meta['repo_sha'][:8] if meta['repo_sha'] else 'unknown'}`, "
               f"binary `{' / '.join(meta['pixel_version'])}`")
    out.append("")
    out.append("| gate | n | TP | FP | FN | TN | precision | recall | F1 |")
    out.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")

    def gate_row(label, c):
        out.append(f"| {label} | {c['n']} | {c['tp']} | {c['fp']} | {c['fn']} | {c['tn']} | "
                   f"{pct(c['precision'])} | {pct(c['recall'])} | {pct(c['f1'])} |")
    gate_row("overall", gate["overall"])
    for field in ("lang", "source"):
        for value, c in gate[f"by_{field}"].items():
            gate_row(f"{field}={value}", c)
    out.append("")
    out.append("| on-topic recall | rows | fired | recall |")
    out.append("| --- | ---: | ---: | ---: |")
    for kind, c in gate["on_topic_recall_by_kind"].items():
        out.append(f"| kind={kind} | {c['n']} | {c['fired']} | {pct(c['recall'])} |")
    out.append("")
    out.append("| off-topic false positives | rows | fired | FP rate |")
    out.append("| --- | ---: | ---: | ---: |")
    out.append(f"| all off-topic | {gate['off_topic_fp']['n']} | {gate['off_topic_fp']['fired']} | "
               f"{pct(gate['off_topic_fp']['fp_rate'])} |")
    out.append(f"| ops + chat | {gate['ops_chat_fp']['n']} | {gate['ops_chat_fp']['fired']} | "
               f"{pct(gate['ops_chat_fp']['fp_rate'])} |")
    for kind, c in gate["off_topic_fp_by_kind"].items():
        out.append(f"| kind={kind} | {c['n']} | {c['fired']} | {pct(c['fp_rate'])} |")
    out.append("")
    out.append("| files (rows with expected_files) | value |")
    out.append("| --- | ---: |")
    out.append(f"| rows / fired | {files['rows']} / {files['fired_rows']} |")
    out.append(f"| recall@{FILES_AT} (unfired = 0) | {pct(files[f'recall@{FILES_AT}'])} |")
    out.append(f"| hit@{FILES_AT} (unfired = miss) | {pct(files[f'hit@{FILES_AT}'])} |")
    out.append(f"| recall@{FILES_AT} when fired | {pct(files[f'recall@{FILES_AT}_when_fired'])} |")
    out.append(f"| hit@{FILES_AT} when fired | {pct(files[f'hit@{FILES_AT}_when_fired'])} |")
    out.append(f"| `files:` line only, recall@{FILES_AT} | {pct(files[f'files_line_recall@{FILES_AT}'])} |")
    out.append(f"| `files:` line only, hit@{FILES_AT} | {pct(files[f'files_line_hit@{FILES_AT}'])} |")
    out.append(f"| any path in the brief, hit | {pct(files['any_section_hit'])} |")
    out.append("")
    out.append(f"| by prompt kind | rows | fired | hit@{FILES_AT} | hit@{FILES_AT} when fired |")
    out.append("| --- | ---: | ---: | ---: | ---: |")
    for kind, c in files["by_kind"].items():
        out.append(f"| kind={kind} | {c['rows']} | {c['fired']} | {pct(c[f'hit@{FILES_AT}'])} | "
                   f"{pct(c[f'hit@{FILES_AT}_when_fired'])} |")
    out.append("")
    out.append("| latency (ms) | n | p50 | p95 | max |")
    out.append("| --- | ---: | ---: | ---: | ---: |")
    for label in ("all", "fired"):
        s = lat[label]
        out.append(f"| {label} | {s['n']} | {ms(s['p50'])} | {ms(s['p95'])} | {ms(s['max'])} |")
    return "\n".join(out)


# --------------------------------------------------------------------------
# Command
# --------------------------------------------------------------------------

def select_rows(rows, split):
    chosen = [r for r in rows if split == "all" or r["split"] == split]
    return sorted(chosen, key=lambda r: r["id"])


def run_bench(args):
    pixel, repo = Path(args.pixel).resolve(), Path(args.repo).resolve()
    if not pixel.is_file():
        raise SystemExit(f"--pixel {pixel}: no such binary")
    if not repo_is_indexed(repo):
        raise SystemExit(f"{repo} has no .pixel/base.shard: run `pixel prepare-repo` there first")
    rows = load_set(args.set)
    problems = validate_rows(rows)
    if problems:
        raise SystemExit("the prompt set is invalid:\n  " + "\n  ".join(problems[:20]))
    chosen = select_rows(rows, args.split)
    repo_sha = git_output(repo, "rev-parse", "HEAD")
    dirty = bool(git_output(repo, "status", "--porcelain"))
    load_before = os.getloadavg()[0] if hasattr(os, "getloadavg") else None
    daemon_before = daemon_running(pixel, repo)
    daemon_now = set_daemon(pixel, repo, args.daemon)
    try:
        ok, why = check_canary(pixel, repo, args.timeout)
        if not ok:
            raise SystemExit("the brief did not fire on a prompt that names a code token "
                             f"({why}): is {repo} indexed at its "
                             "HEAD (`pixel prepare-repo`) and is this binary able to read that index?")
        for _ in range(max(0, args.warmup - 1)):
            run_brief(pixel, repo, CANARY_PROMPT, args.timeout)
        outcomes = {}
        for row in chosen:
            outcomes[row["id"]] = measure_row(pixel, repo, row, args.repeat, args.timeout)
    finally:
        if daemon_now != daemon_before:
            set_daemon(pixel, repo, "on" if daemon_before else "off")
    set_in_repo = (repo / "eval" / "brief-gate").exists()
    result = {
        "meta": {
            "command": ["python3", *sys.argv],
            "started_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "pixel": str(pixel), "pixel_version": version_of(pixel),
            "repo": str(repo), "repo_sha": repo_sha, "repo_dirty": dirty,
            "fixture_sha": FIXTURE_SHA, "fixture_match": repo_sha == FIXTURE_SHA and not dirty,
            "set_in_repo": set_in_repo,
            "split": args.split, "daemon": args.daemon, "daemon_running_during_run": daemon_now,
            "repeat": args.repeat, "warmup": args.warmup, "timeout_s": args.timeout,
            "ollaya_warm": ollaya_warm(),
            "platform": platform.platform(), "python": platform.python_version(),
            "loadavg_1m": {"before": load_before,
                           "after": os.getloadavg()[0] if hasattr(os, "getloadavg") else None},
        },
        "set": {"path": str(args.set), "sha256": hashlib.sha256(Path(args.set).read_bytes()).hexdigest(),
                "rows_run": len(chosen), **summarize_set(rows)},
        "metrics": evaluate(chosen, outcomes),
        "unstable_ids": [i for i, o in sorted(outcomes.items()) if not o["stable"]],
        "error_ids": [i for i, o in sorted(outcomes.items()) if o["rc"] != 0],
        "judge_ran": sorted(i for i, o in outcomes.items() if o["judge"]),
        "rows": [{"id": i, **{k: v for k, v in o.items() if k != "id"}} for i, o in sorted(outcomes.items())],
    }
    if args.out:
        Path(args.out).write_text(json.dumps(result, indent=2, sort_keys=True, ensure_ascii=False) + "\n")
    notes = []
    if not result["meta"]["fixture_match"]:
        notes.append(f"WARNING: repo HEAD {repo_sha} (dirty={dirty}) is not the fixture {FIXTURE_SHA}: "
                     "the expected_files labels were read against the fixture")
    if set_in_repo:
        notes.append("WARNING: eval/brief-gate exists in the repo under test: the set is indexed and "
                     "can answer its own prompts")
    if result["meta"]["ollaya_warm"]:
        notes.append("NOTE: a local Ollaya server is warm: weak-signal prompts may have been judged by it "
                     f"(judge ran on {len(result['judge_ran'])} rows)")
    if result["error_ids"]:
        notes.append(f"WARNING: `pixel brief` exited non-zero or timed out on {len(result['error_ids'])} rows "
                     "(counted as no brief): " + ", ".join(result["error_ids"][:10]))
    if result["unstable_ids"]:
        notes.append(f"WARNING: {len(result['unstable_ids'])} rows changed output between repeats: "
                     + ", ".join(result["unstable_ids"][:10]))
    load = result["meta"]["loadavg_1m"]
    if load["before"] is not None and max(load["before"], load["after"]) > (os.cpu_count() or 1):
        notes.append(f"NOTE: the machine was loaded (1-minute load {load['before']:.1f} before, "
                     f"{load['after']:.1f} after, {os.cpu_count()} CPUs): latency is inflated")
    print(render_markdown(result))
    for note in notes:
        print(f"\n{note}")
    return 0


# --------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------

REAL_FLOW = (
    "[PIXEL:BRIEF]\nkind: flow\nanchors: retrieval_route\n"
    "defined: function retrieval_route crates/pixel/src/execution_brief.rs:199-245 — pub fn retrieval_route(task: &str) -> Value {\n"
    "files: CHANGELOG.md:46 — - **hooks:** ride only prompts; crates/pixel/src/execution_brief.rs:172 —     let route = retrieval_route(x);; "
    "crates/pixel/src/execution_brief.rs:490 — pub fn pretty_retrieval_route(route: &Value) -> String {; (+16 more)\n"
    "callers (impact d1): crates/pixel/src/execution_brief.rs -> from_scope_task:34\n"
    "coverage: 3/3 ops answered\nAnswer from this evidence; open a file only if it contradicts you."
)
REAL_BARE = (
    "[PIXEL:BRIEF]\nkind: lookup\nfiles: crates/a/src/x.rs crates/b/tests/y.rs Cargo.toml (+3 more)\n"
    "tests: crates/b/tests/y.rs\ncoverage: 1/1 ops answered | partial: budget\n"
    "packet partial — open cited regions or run the named op\nnext: pixel find-code 'x'"
)


def row_fixture(**overrides):
    row = {"id": "t-1", "text": "how does install deal with settings", "lang": "en", "on_topic": True,
           "source": "synthetic", "kind": "plain", "split": "dev"}
    row.update(overrides)
    return row


class SelfTest(unittest.TestCase):
    def test_a_non_brief_output_is_not_fired(self):
        self.assertFalse(parse_brief("")["fired"])
        self.assertFalse(parse_brief("{}")["fired"])
        self.assertFalse(parse_brief("some other text [PIXEL:BRIEF]")["fired"])

    def test_files_line_with_texts_keeps_order_and_drops_the_more_tail(self):
        block = parse_brief(REAL_FLOW)
        self.assertTrue(block["fired"])
        self.assertEqual(block["files"], ["CHANGELOG.md", "crates/pixel/src/execution_brief.rs"])
        self.assertEqual(block["defined"], ["crates/pixel/src/execution_brief.rs"])
        self.assertEqual(block["callers"], ["crates/pixel/src/execution_brief.rs"])
        self.assertEqual((block["answered"], block["ops"], block["partial"]), (3, 3, False))

    def test_a_text_holding_a_semicolon_does_not_invent_a_path(self):
        value = "a/b.rs:3 — let x = 1; let y = 2;; c/d.rs — z"
        self.assertEqual(parse_files_line(value), ["a/b.rs", "c/d.rs"])

    def test_bare_paths_are_space_separated_and_partial_is_detected(self):
        block = parse_brief(REAL_BARE)
        self.assertEqual(block["files"], ["crates/a/src/x.rs", "crates/b/tests/y.rs", "Cargo.toml"])
        self.assertEqual(block["tests"], ["crates/b/tests/y.rs"])
        self.assertTrue(block["partial"])

    def test_a_word_is_not_a_path(self):
        self.assertIsNone(looks_like_path("filename"))
        self.assertEqual(looks_like_path("Cargo.toml:12"), "Cargo.toml")
        self.assertEqual(looks_like_path(".github/workflows/ci.yml"), ".github/workflows/ci.yml")

    def test_confusion_counts_and_f1(self):
        c = confusion([(True, True), (True, True), (True, False), (False, True), (False, False), (False, False)])
        self.assertEqual((c["tp"], c["fp"], c["fn"], c["tn"]), (2, 1, 1, 2))
        self.assertAlmostEqual(c["precision"], 2 / 3)
        self.assertAlmostEqual(c["recall"], 2 / 3)
        self.assertAlmostEqual(c["f1"], 2 / 3)

    def test_confusion_without_a_denominator_is_none_not_zero(self):
        c = confusion([(False, False)])
        self.assertIsNone(c["precision"])
        self.assertIsNone(c["recall"])
        self.assertIsNone(c["f1"])

    def test_percentile_is_nearest_rank(self):
        values = list(range(1, 101))
        self.assertEqual(percentile(values, 50), 50)
        self.assertEqual(percentile(values, 95), 95)
        self.assertEqual(percentile([7], 95), 7)
        self.assertIsNone(percentile([], 50))

    def test_file_scores_use_only_the_first_eight_paths(self):
        block = parse_brief("[PIXEL:BRIEF]\nfiles: " + " ".join(f"d/f{i}.rs" for i in range(10)) + "\ncoverage: 1/1 ops answered")
        self.assertEqual(file_scores(["d/f7.rs"], block)["recall"], 1.0)
        late = file_scores(["d/f8.rs"], block)
        self.assertEqual((late["recall"], late["hit"], late["any_hit"]), (0.0, False, True))
        half = file_scores(["d/f0.rs", "d/f9.rs"], block)
        self.assertEqual(half["recall"], 0.5)

    def test_defined_paths_come_first_and_files_line_scores_ignore_them(self):
        block = parse_brief(REAL_FLOW)
        scored = file_scores(["crates/pixel/src/execution_brief.rs", "CHANGELOG.md"], block)
        self.assertEqual(block["defined"] + block["files"][:1], mentioned_paths(block)[:2])
        self.assertEqual(scored["recall"], 1.0)
        defined_only = parse_brief("[PIXEL:BRIEF]\ndefined: function f a/b.rs:1-2\nfiles: c/d.rs\ncoverage: 1/1 ops answered")
        self.assertTrue(file_scores(["a/b.rs"], defined_only)["hit"])
        self.assertFalse(file_scores(["a/b.rs"], defined_only)["files_line_hit"])

    def test_any_section_hit_sees_defined_and_callers(self):
        block = parse_brief(REAL_FLOW)
        self.assertFalse(file_scores(["crates/pixel/src/main.rs"], block)["any_hit"])
        self.assertTrue(file_scores(["crates/pixel/src/execution_brief.rs"], block)["any_hit"])

    def test_evaluate_scores_an_unfired_row_as_a_miss_and_counts_ops_chat_fp(self):
        rows = [row_fixture(id="a", expected_files=["a/b.rs"]),
                row_fixture(id="b", on_topic=False, kind="ops"),
                row_fixture(id="c", on_topic=False, kind="chat", lang="fr"),
                row_fixture(id="d", on_topic=False, kind="paste")]
        fired_block = parse_brief("[PIXEL:BRIEF]\nfiles: a/b.rs\ncoverage: 1/1 ops answered")
        none_block = parse_brief("")
        outcomes = {
            "a": {"fired": False, "block": none_block, "latency_ms": 5},
            "b": {"fired": True, "block": fired_block, "latency_ms": 100},
            "c": {"fired": False, "block": none_block, "latency_ms": 1},
            "d": {"fired": True, "block": fired_block, "latency_ms": 300},
        }
        metrics = evaluate(rows, outcomes)
        self.assertEqual(metrics["gate"]["overall"]["fn"], 1)
        self.assertEqual(metrics["gate"]["ops_chat_fp"], {"n": 2, "fired": 1, "fp_rate": 0.5})
        self.assertEqual(metrics["gate"]["off_topic_fp"]["fired"], 2)
        self.assertEqual(metrics["files"][f"recall@{FILES_AT}"], 0.0)
        self.assertEqual(metrics["files"]["by_kind"]["plain"]["fired"], 0)
        self.assertIsNone(metrics["files"][f"recall@{FILES_AT}_when_fired"])
        self.assertEqual(metrics["latency_ms"]["fired"]["p50"], 100)
        self.assertEqual(metrics["gate"]["by_lang"]["fr"]["n"], 1)
        self.assertEqual(metrics["gate"]["on_topic_recall_by_kind"], {"plain": {"n": 1, "fired": 0, "recall": 0.0}})

    def test_validation_flags_each_contract_break(self):
        self.assertEqual(validate_rows([row_fixture(), row_fixture(id="t-2", text="other", split="test")]), [])
        bad = validate_rows([row_fixture(on_topic=False)])
        self.assertTrue(any("kind" in p for p in bad))
        bad = validate_rows([row_fixture(on_topic=False, kind="ops", expected_files=["a.rs"])])
        self.assertTrue(any("off-topic row" in p for p in bad))
        bad = validate_rows([row_fixture(expected_files=["/abs/a.rs"])])
        self.assertTrue(any("repo-relative" in p for p in bad))
        bad = validate_rows([row_fixture(expected_files=["a/missing.rs"])], repo_files={"a/x.rs"})
        self.assertTrue(any("does not exist" in p for p in bad))
        dup = validate_rows([row_fixture(), row_fixture()])
        self.assertTrue(any("duplicate id" in p for p in dup))
        unbalanced = validate_rows([row_fixture(id=f"u{i}", text=f"t{i}") for i in range(4)])
        self.assertTrue(any("unbalanced" in p for p in unbalanced))

    def test_privacy_lint_catches_what_a_public_set_must_not_hold(self):
        for value in ("mail me at someone@example.org", "see /Users/someone/project", "ssh 192.168.0.12",
                      "key ghp_abcdefghijklmnop1234", "https://example.com/private",
                      "token " + "A1b2C3d4" * 6):
            self.assertTrue(privacy_problems("text", value), value)
        for value in ("how does install work", "https://github.com/Pixel-CLI/pixel/pull/876",
                      "crates/pixel-install/src/config.rs"):
            self.assertEqual(privacy_problems("text", value), [], value)

    def test_shipped_set_is_valid(self):
        if not DEFAULT_SET.is_file():
            self.skipTest("no prompt set next to this script")
        self.assertEqual(validate_rows(load_set(DEFAULT_SET)), [])

    def test_the_brief_is_forced_on_for_every_call(self):
        saved = os.environ.get("PIXEL_BRIEF")
        os.environ["PIXEL_BRIEF"] = "0"
        try:
            self.assertEqual(brief_env()["PIXEL_BRIEF"], "1")
        finally:
            if saved is None:
                del os.environ["PIXEL_BRIEF"]
            else:
                os.environ["PIXEL_BRIEF"] = saved

    def test_pipeline_against_a_stub_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            repo = tmp / "repo"
            (repo / ".pixel").mkdir(parents=True)
            (repo / ".pixel" / "base.shard").write_text("x")
            stub = tmp / "pixel"
            stub.write_text(
                "#!/bin/sh\n"
                'case "$1" in\n'
                '  --version) echo "pixel 0.0.0"; echo "commit: stub";;\n'
                '  daemon) echo "daemon not running for x";;\n'
                '  brief) case "$4" in *install*) printf "[PIXEL:BRIEF]\\nfiles: a/b.rs\\ncoverage: 1/1 ops answered";; esac;;\n'
                "esac\n")
            stub.chmod(0o755)
            on_row = row_fixture(id="a", text="how does install work", expected_files=["a/b.rs"])
            off_row = row_fixture(id="b", text="good morning", on_topic=False, kind="chat", split="test")
            outcomes = {r["id"]: measure_row(stub, repo, r, 2, 10) for r in (on_row, off_row)}
            self.assertTrue(outcomes["a"]["fired"])
            self.assertFalse(outcomes["b"]["fired"])
            self.assertTrue(outcomes["a"]["stable"])
            metrics = evaluate([on_row, off_row], outcomes)
            self.assertEqual(metrics["gate"]["overall"]["tp"], 1)
            self.assertEqual(metrics["files"][f"recall@{FILES_AT}"], 1.0)
            self.assertFalse(daemon_running(stub, repo))
            self.assertTrue(repo_is_indexed(repo))
            self.assertEqual(check_canary(stub, repo, 10)[0], False)  # the stub only answers `install`
            self.assertIn("exit 0", check_canary(stub, repo, 10)[1])


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
    outcome = unittest.TextTestRunner(verbosity=1).run(suite)
    return 0 if outcome.wasSuccessful() else 1


def check_set(args):
    rows = load_set(args.set)
    repo_files = tracked_files(args.repo) if args.repo else None
    if args.repo and not repo_files:
        raise SystemExit(f"{args.repo}: no tracked files (is it a git checkout?)")
    problems = validate_rows(rows, repo_files)
    print(json.dumps(summarize_set(rows), sort_keys=True))
    for problem in problems:
        print(f"problem: {problem}")
    return 1 if problems else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--pixel", help="the pixel binary to measure")
    parser.add_argument("--repo", help="an indexed checkout (pixel prepare-repo) at the fixture SHA")
    parser.add_argument("--set", default=str(DEFAULT_SET), help="prompt set (JSONL)")
    parser.add_argument("--split", choices=("dev", "test", "all"), default="dev")
    parser.add_argument("--daemon", choices=("on", "off"), default="off",
                        help="run with the repo's daemon started or stopped (restored afterwards)")
    parser.add_argument("--out", help="write the full result as JSON here")
    parser.add_argument("--repeat", type=int, default=1, help="calls per prompt; latency is the median, outputs must agree")
    parser.add_argument("--warmup", type=int, default=1, help="untimed calls before the first prompt (the first is the canary)")
    parser.add_argument("--timeout", type=float, default=15.0, help="seconds per call")
    parser.add_argument("--check-set", action="store_true", help="validate the set (and its files with --repo) and exit")
    parser.add_argument("--self-test", action="store_true", help="run the parser and metric tests and exit")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.check_set:
        return check_set(args)
    if not (args.pixel and args.repo):
        parser.error("--pixel and --repo are required")
    return run_bench(args)


if __name__ == "__main__":
    sys.exit(main())
