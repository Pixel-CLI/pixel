#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Judge a brief probe (issue #906): does a script that adds evidence to the prompt-submit brief remove tool calls?

A probe is a directory ``probes/<name>/`` with a ``pixel-plugin.toml`` (``capabilities = ["brief-probe"]`` and a
``[brief-probe]`` table) and an executable ``probe`` named by ``run``. The brief calls it with JSON on stdin,
``{prompt, repo_root, brief, kind, session_files}``, and takes brief blocks from its stdout:

    ### path:lo-hi — label        a block: this header line, then the lines under it
    drop: <regex>                 remove the brief's blocks whose header (after ``### ``) matches
    first: <regex>                move the brief's blocks whose header matches to the front

A probe is read-only, uses no network and is deterministic (the same input gives the same bytes). ``.env*``
files may be read only when git tracks them, with secret-looking values masked. ``mode = "lead"`` puts its
blocks before the brief's, ``"append"`` after; ``budget_ms`` (at most 2500) is the wall time it gets, and a
probe that overruns, fails or prints nonsense is skipped.

    probe-test.py <probe-dir-or-name> [--questions Q.jsonl | --census-db DB] [--bin PIXEL] [--ids a,b]
                  [--report-dir D] [--live] [--live-jobs 4]
    probe-test.py --self-test

Offline (seconds): schema, budget, determinism (twice), read-only, bytes, oracle coverage before -> after, the
call replay of ``brief-check.py`` and the neighbours (every question the probe does not target) that must not get
worse. ``--live`` runs Codex on the probe's target questions with and without it, in parallel. The verdict is
KEEP or DROP, printed as one line and written as ``report.md`` and ``report.json``. The merge below is the
specification the brief itself implements (``execution_brief/probes.rs``). Standard library only; ``python3 -I``.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parent
MAX_BUDGET_MS = 2500
MODES = ("append", "lead")
KINDS = ("overview", "review", "flow", "history", "config", "lookup", "tests", "impact", "diagnosis",
         "error-origin", "feature", "bugfix", "rationale", "other")
PART_CHARS = 9_800
PARTS = 3
FOOTER_PREFIXES = ("Answer from this context.", "These are the regions closest", "pack incomplete",
                   "confidence:")
CAPABILITY = "brief-probe"
SECRET_KEY = re.compile(r"key|secret|token|password|passwd|credential|private|signature", re.I)


def load_bc():
    spec = importlib.util.spec_from_file_location("brief_check", HERE / "brief-check.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules["brief_check"] = module
    spec.loader.exec_module(module)
    return module


# --------------------------------------------------------------------------- the contract

class Probe:
    def __init__(self, directory):
        self.dir = Path(directory).resolve()
        self.problems = []
        try:
            doc = tomllib.loads((self.dir / "pixel-plugin.toml").read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as error:
            doc = {}
            self.problems.append(f"pixel-plugin.toml: {error}")
        table = doc.get("brief-probe") or {}
        self.name = str(doc.get("name", ""))
        self.owner = str(table.get("owner", ""))
        self.kinds = list(table.get("when_kinds") or [])
        self.skip = list(table.get("skip_kinds") or [])
        self.pattern = str(table.get("when_prompt") or "")
        self.budget_ms = table.get("budget_ms", 0)
        self.mode = table.get("mode", "append")
        self.run = str(doc.get("run", "probe"))
        self.check(doc, table)

    def check(self, doc, table):
        problems = self.problems
        if doc.get("name") != self.dir.name:
            problems.append(f"name `{doc.get('name')}` must equal the directory `{self.dir.name}`")
        if doc.get("api") != 1:
            problems.append("api must be 1")
        for key in ("version", "description", "run"):
            if not isinstance(doc.get(key), str) or not doc.get(key):
                problems.append(f"`{key}` is required")
        if doc.get("capabilities") != [CAPABILITY]:
            problems.append(f'capabilities must be ["{CAPABILITY}"]')
        if doc.get("network"):
            problems.append("a probe uses no network (`network` must be absent or false)")
        if not self.owner:
            problems.append("[brief-probe] owner is required")
        if not isinstance(self.budget_ms, int) or not 0 < self.budget_ms <= MAX_BUDGET_MS:
            problems.append(f"budget_ms must be an integer in 1..={MAX_BUDGET_MS}")
        if self.mode not in MODES:
            problems.append(f"mode must be one of {MODES}")
        for key, kinds in (("when_kinds", self.kinds), ("skip_kinds", self.skip)):
            for kind in kinds:
                if kind not in KINDS:
                    problems.append(f"{key}: unknown kind `{kind}`")
        if self.pattern:
            try:
                re.compile(self.pattern)
            except re.error as error:
                problems.append(f"when_prompt is not a regex: {error}")
        program = self.dir / self.run
        if not program.is_file() or not os.access(program, os.X_OK):
            problems.append(f"`{self.run}` is not an executable file")
        for extra in sorted(set(table) - {"owner", "when_kinds", "skip_kinds", "when_prompt", "budget_ms", "mode"}):
            problems.append(f"[brief-probe] unknown key `{extra}`")

    def applies(self, kind, prompt):
        """``when``: a kind in ``when_kinds`` or the prompt regex; with neither, always. A kind in ``skip_kinds``
        never runs it. The kind is ``None`` when the brief did not settle on one (a weak, low-tier brief)."""
        if kind in self.skip:
            return False
        if not self.kinds and not self.pattern:
            return True
        return (kind in self.kinds) or bool(self.pattern and re.search(self.pattern, prompt))


def run_probe(probe, request, pixel_bin, home):
    """``(stdout, elapsed ms, error)``: the probe run the way the brief runs it, within its budget."""
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "LANG": "C.UTF-8",
           "PIXEL_BIN": str(pixel_bin), "GIT_CONFIG_NOSYSTEM": "1"}
    started = time.monotonic()
    try:
        done = subprocess.run([str(probe.dir / probe.run)], input=json.dumps(request), capture_output=True, text=True,
                              cwd=request["repo_root"], env=env, timeout=probe.budget_ms / 1000)
    except subprocess.TimeoutExpired:
        return "", probe.budget_ms, "timeout"
    except OSError as error:
        return "", 0, f"spawn: {error}"
    ms = int((time.monotonic() - started) * 1000)
    if done.returncode != 0:
        return "", ms, f"exit {done.returncode}: {done.stderr.strip()[-200:]}"
    return done.stdout, ms, None


# --------------------------------------------------------------------------- the merge (the specification)

def split_brief(text):
    """``(head, blocks, footer)`` of a brief: lines before the first ``### ``, the blocks (each a list of lines
    from its header), and the trailing lines that are directives or hedges, kept last whatever happens."""
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    footer = []
    while lines and lines[-1].startswith(FOOTER_PREFIXES):
        footer.insert(0, lines.pop())
    head, blocks = [], []
    for line in lines:
        if line.startswith("### "):
            blocks.append([line])
        elif blocks:
            blocks[-1].append(line)
        else:
            head.append(line)
    return head, blocks, footer


def parse_output(text):
    """``(blocks, drops, firsts, stray)`` of a probe's stdout. ``stray`` counts lines that belong to no block."""
    blocks, drops, firsts, stray = [], [], [], 0
    for line in text.split("\n"):
        if line.startswith("### "):
            blocks.append([line])
        elif line.startswith("drop: "):
            drops.append(line[6:])
        elif line.startswith("first: "):
            firsts.append(line[7:])
        elif blocks:
            blocks[-1].append(line)
        elif line.strip():
            stray += 1
    for block in blocks:
        while block and block[-1] == "":
            block.pop()
    return blocks, drops, firsts, stray


def header_of(block):
    return block[0][4:]


def safe_search(pattern, text):
    try:
        return re.search(pattern, text) is not None
    except re.error:
        return False


def merge(brief, outputs, cap=PART_CHARS * PARTS):
    """The brief after its probes. ``outputs``: ``[(name, mode, stdout)]`` of the probes that ran, any order (they
    are applied by name). Drops and firsts act on the brief's own blocks only; a probe's block whose header the
    brief already has is skipped; when the text is over ``cap`` characters the append blocks go first, then the
    brief's last blocks, then the lead blocks, whole, and the first block is never removed."""
    head, existing, footer = split_brief(brief)
    if not head:
        head = ["[PIXEL:BRIEF]"]
    lead, append, drops, firsts = [], [], [], []
    for name, mode, stdout in sorted(outputs, key=lambda o: o[0]):
        blocks, d, f, _ = parse_output(stdout)
        drops += d
        firsts += f
        (lead if mode == "lead" else append).extend(blocks)
    existing = [b for b in existing if not any(safe_search(rx, header_of(b)) for rx in drops)]
    for rx in reversed(firsts):
        moved = [b for b in existing if safe_search(rx, header_of(b))]
        existing = moved + [b for b in existing if b not in moved]
    held = {header_of(b) for b in existing}
    for blocks in (lead, append):
        fresh = []
        for block in blocks:
            if header_of(block) not in held:
                held.add(header_of(block))
                fresh.append(block)
        blocks[:] = fresh
    order = [("lead", b) for b in lead] + [("brief", b) for b in existing] + [("append", b) for b in append]

    def render(items):
        return "\n".join(head + [line for _, b in items for line in b] + footer) + "\n"

    for victim in ("append", "brief", "lead"):
        while len(render(order)) > cap and len(order) > 1:
            index = max((i for i, (who, _) in enumerate(order) if who == victim), default=None)
            if index is None or (index == 0 and len(order) == 1):
                break
            order.pop(index)
    if len(render(order)) > cap and order:
        # One block is left and it is still too long: keep its leading lines, never the directive after it.
        who, block = order[-1]
        while len(block) > 1 and len(render(order[:-1] + [(who, block)])) > cap:
            block = block[:-1]
        order[-1] = (who, block)
    return render(order)


# --------------------------------------------------------------------------- question sets

class CensusSet:
    """The private census questions, from ``census.db`` and the recorded runs (``--census-db``, local only)."""
    name = "census"

    def __init__(self, bc, sb, args, binary, source):
        self.bc, self.sb, self.root, self.binary, self.source = bc, sb, Path(args.root), binary, source
        self.history = not args.no_history

    def ids(self):
        return self.source.ids()

    def open(self, qid):
        repo_name, prompt, clone = self.bc.prepare(self.source, self.root, qid, self.binary, history=self.history)
        text, meta, ms = self.bc.run_brief(self.binary, clone, prompt, tries=3)
        return {"id": qid, "repo": repo_name, "prompt": prompt, "clone": str(clone), "text": text, "meta": meta,
                "ms": ms, "kind": meta.get("kind")}

    def score(self, q, text):
        row = self.bc.score(self.source, self.root, self.sb, q["id"], q["clone"], text, q["ms"], q["meta"])
        return {"calls": self.bc.residual_of(row), "quoted": row["nb_quoted"], "bytes": row["bytes"]}


def load_bench(path):
    """The public bench: ``questions.jsonl`` ({id, repo, sha, prompt, kind, oracle}) and ``baselines.jsonl``."""
    rows = [json.loads(line) for line in Path(path).joinpath("questions.jsonl").read_text().splitlines() if line.strip()]
    base = {}
    baselines = Path(path) / "baselines.jsonl"
    if baselines.exists():
        for line in baselines.read_text().splitlines():
            if line.strip():
                rec = json.loads(line)
                base[rec["id"]] = rec
    return rows, base


class BenchSet:
    """``eval/brief-bench``: public questions on pinned public repositories, with recorded no-brief aggregates."""
    name = "bench"

    def __init__(self, bc, sb, args, binary):
        self.bc, self.sb, self.binary = bc, sb, binary
        self.dir = Path(args.bench)
        self.rows, self.baselines = load_bench(self.dir)
        self.cache = Path(args.cache)

    def ids(self):
        return [r["id"] for r in self.rows]

    def clone(self, row):
        dest = self.cache / "clones" / f"{row['repo'].replace('/', '__')}-{row['sha'][:8]}"
        marker = dest / ".pixel" / "probe-test.json"
        digest = hashlib.sha256(Path(self.binary).read_bytes()).hexdigest()
        if marker.exists() and json.loads(marker.read_text()).get("sha256") == digest:
            return dest
        import fcntl
        dest.parent.mkdir(parents=True, exist_ok=True)
        with open(dest.parent / f".lock-{dest.name}", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if not (dest / ".git").exists():
                url = row.get("url") or f"https://github.com/{row['repo']}.git"
                subprocess.run(["git", "clone", "-q", url, str(dest)], check=True, capture_output=True)
            # Pinned at the question's commit before anything is indexed, whatever state an earlier run left.
            head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=dest, capture_output=True, text=True).stdout.strip()
            if head != row["sha"]:
                subprocess.run(["git", "checkout", "-q", "--detach", row["sha"]], cwd=dest, check=True, capture_output=True)
            env = self.bc.env_for_brief()
            for argv in (["daemon", "stop", "."], ["prepare-repo", "."], ["build-index", "--history", "."]):
                subprocess.run([self.binary, *argv], cwd=dest, capture_output=True, env=env, timeout=1800)
            subprocess.run([self.binary, "search-meaning", "how does this code work", "."], cwd=dest,
                           capture_output=True, env=env, timeout=1800)
            marker.parent.mkdir(parents=True, exist_ok=True)
            marker.write_text(json.dumps({"sha256": digest}))
        return dest

    def open(self, qid):
        row = next(r for r in self.rows if r["id"] == qid)
        clone = self.clone(row)
        text, meta, ms = self.bc.run_brief(self.binary, clone, row["prompt"], tries=3)
        return {"id": qid, "repo": row["repo"], "prompt": row["prompt"], "clone": str(clone), "text": text,
                "meta": meta, "ms": ms, "kind": meta.get("kind") or row.get("kind"), "row": row}

    def score(self, q, text):
        repo = self.sb.Repo(q["clone"])
        regions = self.sb.parse_brief(text, repo)
        quoted = self.bc.quoted_share(self.sb, repo, regions, None, q["row"].get("oracle"))
        residual = sum(1 for call in (self.baselines.get(q["id"]) or {}).get("calls", [])
                       if self.bc.judge_summary(regions, call)[0] != "covered")
        return {"calls": residual, "quoted": quoted, "bytes": len(text.encode())}


# --------------------------------------------------------------------------- the overfitting guard

COMMON = set("""about above after again against also another because been before being below between both could does
doing down during each from further have having here into just keep keeps more most much once only other over same
should some such than that their them then there these they this those through under until very want what when
where which while will with would your how why who its are and the for not you can any all but was has our""".split())


def literals_of_prompt(prompt, oracle_paths=()):
    """What a probe written for ONE question would hard-code: its content words (five letters or more) and the
    paths, file names and stems of the evidence that question needed."""
    words = {w.lower() for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]{4,}", prompt)} - COMMON
    lits = set(words)
    for path in oracle_paths:
        path = str(path)
        lits.add(path)
        base = path.rsplit("/", 1)[-1]
        lits.add(base)
        stem = base.rsplit(".", 1)[0]
        if len(stem) >= 4:
            lits.add(stem)
    return {x for x in lits if x}


def overfit(probe_dir, prompt, oracle_paths=(), others=None):
    """Literals of ``prompt`` (and of its evidence) that the probe's code spells out, sorted. A word the other
    probes of ``probes/`` already use is vocabulary of the contract (``config``, ``history``) and is allowed; a
    probe must derive the rest from the prompt and the repository when it runs."""
    probe_dir = Path(probe_dir)
    source = "\n".join(p.read_text(errors="replace") for p in sorted(probe_dir.iterdir())
                       if p.is_file() and p.suffix != ".toml" and not p.name.startswith("."))
    vocabulary = ""
    for other in sorted((others or probe_dir.parent).glob("*/probe")):
        if other.parent.resolve() != probe_dir.resolve():
            vocabulary += other.read_text(errors="replace").lower() + "\n"
    found = []
    for lit in sorted(literals_of_prompt(prompt, oracle_paths)):
        pattern = rf"(?<![A-Za-z0-9_]){re.escape(lit)}(?![A-Za-z0-9_])"
        if re.search(pattern, source, re.I) and not (len(lit) < 20 and "/" not in lit and re.search(pattern, vocabulary)):
            found.append(lit)
    return found


# --------------------------------------------------------------------------- the offline evaluation

def snapshot(clone):
    """What read-only means: the working tree status and the tracked files' size and mtime."""
    status = subprocess.run(["git", "status", "--porcelain"], cwd=clone, capture_output=True, text=True).stdout
    stat = subprocess.run("git ls-files -z | xargs -0 stat -f '%N %z %m' 2>/dev/null || true", shell=True, cwd=clone,
                          capture_output=True, text=True).stdout
    return hashlib.sha256((status + stat).encode()).hexdigest()


def evaluate(probe, qset, pixel_bin, ids, home):
    """One row per question: the baseline brief and the merged one, scored the same way."""
    rows = []
    for qid in ids:
        q = qset.open(qid)
        gate_open = (q["meta"].get("gate") or "open") == "open"
        request = {"prompt": q["prompt"], "repo_root": q["clone"], "brief": q["text"] or "[PIXEL:BRIEF]\n",
                   "kind": q["kind"], "session_files": []}
        applies = gate_open and probe.applies(q["kind"], q["prompt"])
        row = {"id": qid, "repo": q["repo"], "kind": q["kind"], "applies": applies, "ran": False, "error": None,
               "probe_ms": 0, "deterministic": True, "read_only": True, "stray": 0}
        before = qset.score(q, q["text"])
        row["before"] = before
        after, text = before, q["text"]
        if applies:
            snap = snapshot(q["clone"])
            first, ms, error = run_probe(probe, request, pixel_bin, home)
            second, _, _ = run_probe(probe, request, pixel_bin, home) if not error else ("", 0, None)
            row.update(probe_ms=ms, error=error, ran=error is None and bool(first.strip()),
                       deterministic=first == second, read_only=snapshot(q["clone"]) == snap)
            row["stray"] = parse_output(first)[3]
            if row["ran"]:
                text = merge(q["text"] or "[PIXEL:BRIEF]\n", [(probe.name, probe.mode, first)])
                after = qset.score(q, text)
        row["after"] = after
        row["text"] = text
        rows.append(row)
    return rows


def verdict_of(probe, rows, live=None):
    """``(verdict, reasons)``. Offline the verdict is ``OK`` or ``FAIL``: the contract holds, the probe is
    deterministic, read-only and inside its budget, and no question it touched got worse. The replay cannot see
    most gains (the lines a probe adds are not the lines the recorded session read), so a gain is not required
    offline. With ``live`` the verdict is ``KEEP`` or ``DROP``: the offline checks must pass and the median
    calls of no target question may rise, at least one must fall."""
    reasons = list(probe.problems)
    ran = [r for r in rows if r["ran"]]
    reasons += [f"{r['id']}: probe error {r['error']}" for r in rows if r["error"]]
    reasons += [f"{r['id']}: not deterministic" for r in rows if not r["deterministic"]]
    reasons += [f"{r['id']}: modified the repository" for r in rows if not r["read_only"]]
    reasons += [f"{r['id']}: {r['stray']} stray line(s) outside any block" for r in rows if r["stray"]]
    reasons += [f"{r['id']}: took {r['probe_ms']} ms of {probe.budget_ms}" for r in ran if r["probe_ms"] >= probe.budget_ms]
    worse = [r for r in ran if r["after"]["calls"] > r["before"]["calls"]
             or r["after"]["quoted"] < r["before"]["quoted"] - 0.05]
    reasons += [f"{r['id']}: worse ({r['before']['calls']} -> {r['after']['calls']} calls, quoted "
                f"{r['before']['quoted']:.0%} -> {r['after']['quoted']:.0%})" for r in worse]
    if not ran and not probe.problems:
        reasons.append("the probe added nothing to any question")
    if live is None:
        return ("FAIL" if reasons else "OK"), reasons
    failed = [l for l in live if l["with"] < 0 or l["without"] < 0]
    reasons += [f"{l['id']}: a live arm did not complete (with {l['with_runs']}, without {l['without_runs']})"
                for l in failed]
    live = [l for l in live if l not in failed]
    bad = [l for l in live if l["with"] > l["without"]]
    reasons += [f"{l['id']}: live median {l['without']} -> {l['with']} calls" for l in bad]
    if live and not any(l["with"] < l["without"] for l in live) and not bad:
        reasons.append("live: the median of no target question dropped")
    return ("DROP" if reasons else "KEEP"), reasons


def verdict_line(probe, rows, verdict, live):
    targets = [r for r in rows if r["ran"]]
    b = sum(r["before"]["calls"] for r in targets)
    a = sum(r["after"]["calls"] for r in targets)
    qb = sum(r["before"]["quoted"] for r in targets) / len(targets) if targets else 0.0
    qa = sum(r["after"]["quoted"] for r in targets) / len(targets) if targets else 0.0
    line = (f"PROBE {probe.name} | {','.join(r['id'] for r in targets) or '-'} | predicted calls {b} -> {a} | "
            f"coverage {qb:.0%} -> {qa:.0%}")
    if live is not None:
        line += f" | live {sum(l['without'] for l in live)} -> {sum(l['with'] for l in live)} calls"
    return f"{line} | {verdict}"


def write_report(directory, probe, rows, verdict, reasons, live):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    slim = [{k: v for k, v in r.items() if k != "text"} for r in rows]
    (directory / "report.json").write_text(json.dumps(
        {"probe": probe.name, "owner": probe.owner, "mode": probe.mode, "budget_ms": probe.budget_ms,
         "verdict": verdict, "reasons": reasons, "rows": slim, "live": live}, indent=1) + "\n")
    out = [f"# Probe `{probe.name}`: {verdict}", "", f"owner {probe.owner}, mode {probe.mode}, budget {probe.budget_ms} ms", ""]
    out += [f"- {r}" for r in reasons] or ["- every check passed"]
    out += ["", "| question | kind | ran | ms | calls | quoted | bytes |", "|---|---|---|---:|---|---|---|"]
    for r in rows:
        out.append(f"| {r['id']} | {r['kind']} | {'yes' if r['ran'] else ('no' if r['applies'] else 'n/a')} | {r['probe_ms']} | "
                   f"{r['before']['calls']} -> {r['after']['calls']} | {r['before']['quoted']:.0%} -> "
                   f"{r['after']['quoted']:.0%} | {r['before']['bytes']} -> {r['after']['bytes']} |")
    if live is not None:
        out += ["", "## Live (Codex, median and min-max over the runs of each arm)", "", "| question | without | with |", "|---|---:|---:|"]
        out += [f"| {l['id']} | {l['without']} ({min(l['without_runs'])}-{max(l['without_runs'])}) | "
                f"{l['with']} ({min(l['with_runs'])}-{max(l['with_runs'])}) |" for l in live]
    (directory / "report.md").write_text("\n".join(out) + "\n")


# --------------------------------------------------------------------------- live: Codex with and without

SHIM = """#!/bin/sh
exec python3 -I "{script}" --hook-shim "{config}" "$@"
"""


def make_shim(directory, probe, pixel_bin):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    config = directory / "shim.json"
    config.write_text(json.dumps({"probe": str(probe.dir), "pixel": str(pixel_bin)}))
    shim = directory / "pixel"
    shim.write_text(SHIM.format(script=Path(__file__).resolve(), config=config))
    shim.chmod(0o755)
    return shim


def hook_shim(config_path, args):
    """The ``pixel`` a live session calls: the real binary, and on part 1 of the prompt-submit hook the probe's
    blocks merged into the brief. Parts after the first print the slices part 1 left behind."""
    config = json.loads(Path(config_path).read_text())
    real = config["pixel"]
    if not (args[:2] == ["run-hook", "task-event"] and "prompt-submit" in args):
        os.execv(real, [real, *args])
    probe = Probe(config["probe"])
    stdin = sys.stdin.read()
    try:
        payload = json.loads(stdin)
    except ValueError:
        payload = {}
    prompt = payload.get("prompt", "")
    part = int(args[args.index("--part") + 1].split("/")[0]) if "--part" in args else 1
    cache = Path.cwd() / ".pixel" / ("probe-shim-" + hashlib.sha256(prompt.encode()).hexdigest()[:16] + ".json")
    done = Path(str(cache) + ".done")

    def envelope(ctx):
        return json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}})

    if part > 1:
        deadline = time.time() + 8
        while time.time() < deadline and not done.exists():
            time.sleep(0.05)
        try:
            state = json.loads(done.read_text())
        except (OSError, ValueError):
            state = {"mode": "pass"}
        if state["mode"] == "pieces":
            pieces = state["pieces"]
            sys.stdout.write(envelope(pieces[part - 1]) if part - 1 < len(pieces) else "{}")
            return 0
        proc = subprocess.run([real, *args], input=stdin, capture_output=True, text=True)
        sys.stdout.write(proc.stdout)
        return 0
    done.unlink(missing_ok=True)
    proc = subprocess.run([real, *args], input=stdin, capture_output=True, text=True)
    out, state = proc.stdout, {"mode": "pass"}

    def context_of(text):
        try:
            return json.loads(text)["hookSpecificOutput"].get("additionalContext", "") if text.strip() else ""
        except (ValueError, KeyError, TypeError):
            return ""

    try:
        body = json.loads(out) if out.strip() else {"hookSpecificOutput": {"hookEventName": "UserPromptSubmit",
                                                                            "additionalContext": ""}}
        # The probe reads the WHOLE brief: the real parts 2..N are joined to part 1's slice (a probe that saw only
        # part 1 and replaced it dropped the other parts), then the merge is split over the parts again.
        count = int(args[args.index("--part") + 1].split("/")[1]) if "--part" in args else 1
        context = body["hookSpecificOutput"].get("additionalContext", "")
        for k in range(2, count + 1):
            later = [f"{k}/{count}" if a == args[args.index("--part") + 1] else a for a in args]
            context += context_of(subprocess.run([real, *later], input=stdin, capture_output=True, text=True).stdout)
        kind, gate = last_decision(Path.cwd())
        if gate == "open" and probe.applies(kind, prompt):
            request = {"prompt": prompt, "repo_root": str(Path.cwd()), "brief": context or "[PIXEL:BRIEF]\n",
                       "kind": kind, "session_files": []}
            stdout, _, error = run_probe(probe, request, real, os.environ.get("HOME", "/"))
            if not error and stdout.strip():
                merged = merge(context or "[PIXEL:BRIEF]\n", [(probe.name, probe.mode, stdout)])
                if merged != context:
                    pieces = split_pieces(merged)
                    state = {"mode": "pieces", "pieces": pieces}
                    body["hookSpecificOutput"]["additionalContext"] = pieces[0] if pieces else ""
                    out = json.dumps(body)
    except (KeyError, ValueError):
        pass
    cache.parent.mkdir(parents=True, exist_ok=True)
    done.write_text(json.dumps(state))
    sys.stdout.write(out)
    return 0


def last_decision(root):
    try:
        lines = [l for l in (root / ".pixel" / "brief-decisions.jsonl").read_text().splitlines() if l.strip()]
        rec = json.loads(lines[-1])
        return rec.get("kind"), rec.get("gate")
    except (OSError, ValueError, IndexError):
        return None, None


def split_pieces(text, cap=PART_CHARS):
    pieces, current = [], ""
    for line in text.splitlines(True):
        if len(current) + len(line) > cap and current:
            pieces.append(current)
            current = ""
        current += line[:cap]
    return pieces + ([current] if current else [])


def median(values):
    values = sorted(values)
    mid = len(values) // 2
    return values[mid] if len(values) % 2 else (values[mid - 1] + values[mid]) / 2


def live_run(probe, qset, pixel_bin, ids, args, report_dir):
    """The agent (``--agent``) on each target question ``--live-reps`` times through the shim (with the probe) and the plain binary
    (without), every run in parallel: one agent run varies a lot (the same empty brief took 8 and 11 calls). The
    result per question is the median and the min-max of each arm; the verdict reads the medians."""
    shim = make_shim(Path(report_dir) / "live", probe, pixel_bin)
    stamp = int(time.time())

    def one_census(arm, rep, binary):
        run_id = f"probe-{probe.name}-{arm}{rep}-{stamp}"
        cmd = ["python3", "-I", str(HERE / "brief-zero-calls.py"), "--brief-bin", str(binary), "--agent", args.agent,
               "--arm", "brief", "--only", ",".join(ids), "--jobs", str(args.live_jobs), "--run-id", run_id,
               "--root", args.root, "--census-db", args.census_db]
        env = {k: v for k, v in os.environ.items() if k != "ANTHROPIC_API_KEY"}
        subprocess.run(cmd, capture_output=True, text=True, env=env)
        report = Path(args.root) / "zero-calls" / run_id / "report.json"
        rows = json.loads(report.read_text())["rows"]
        # The shim replaces what parts 2 and 3 print, so the harness's own "hooks ran 1/3" is not a failure here.
        return arm, {r["id"]: r["n_calls"] for r in rows if r["n_calls"] >= 0}

    def one_bench(arm, rep, binary):
        bb = load_module("brief_bench", "brief-bench.py")
        rows = [r for r in qset.rows if r["id"] in ids]
        ns = argparse.Namespace(jobs=args.live_jobs, timeout=900, model="gpt-5.6-terra", effort="medium",
                                root=args.root, census_dir=args.census_dir, agent=args.agent,
                                claude_model=None, claude_effort=None)
        done = bb.run_codex(ns, rows, 1, binary=str(binary), src=lambda q: qset.clone(q), tag=f"probe-{arm}{rep}")
        return arm, {qid: rec["n_calls"] for qid, _r, rec in done if rec}

    one = one_census if qset.name == "census" else one_bench
    runs = {"with": [], "without": []}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2 * args.live_reps) as pool:
        futures = [pool.submit(one, arm, rep, shim if arm == "with" else pixel_bin)
                   for rep in range(args.live_reps) for arm in ("with", "without")]
        for future in futures:
            arm, counts = future.result()
            runs[arm].append(counts)
    out = []
    for q in ids:
        row = {"id": q}
        for arm in ("without", "with"):
            values = [r[q] for r in runs[arm] if q in r]
            row[arm] = median(values) if values else -1
            row[arm + "_runs"] = values
        out.append(row)
    return out


def load_module(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


# --------------------------------------------------------------------------- main

def resolve_probe(text):
    path = Path(text)
    return path if path.is_dir() else WORKTREE / "probes" / text


def check_all(probes_dir=None):
    """Contract check of every probe of ``probes/`` with no question set and no network: the manifest, then each probe
    run twice on a small throwaway git repository (exit 0, within its budget, the same bytes, nothing but blocks and
    directives, the repository untouched). Returns the problems, empty when every probe holds."""
    probes_dir = Path(probes_dir or WORKTREE / "probes")
    problems = []
    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp) / "repo"
        repo.mkdir()
        (repo / "src").mkdir()
        (repo / "src" / "lib.py").write_text("DEFAULT_ROWS = 100\n\ndef search(rows=DEFAULT_ROWS):\n    return rows\n")
        (repo / "README.md").write_text("# Demo\n\nHow the search default rows setting works.\n")
        env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_GLOBAL": "/dev/null"}
        for argv in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "search default rows"]):
            subprocess.run(["git", *argv], cwd=repo, env=env, capture_output=True)
        for directory in sorted(d for d in probes_dir.glob("*/") if not d.name.startswith(("_", "."))):
            probe = Probe(directory)
            problems += [f"{directory.name}: {p}" for p in probe.problems]
            if probe.problems:
                continue
            request = {"prompt": "what is the default number of rows a search returns", "repo_root": str(repo),
                       "brief": "[PIXEL:BRIEF]\n### src/lib.py:1-4 — DEFAULT_ROWS (constant)\n1| DEFAULT_ROWS = 100\n",
                       "kind": "lookup", "session_files": []}
            before = snapshot(repo)
            first, ms, error = run_probe(probe, request, "pixel", tmp)
            second, _, _ = run_probe(probe, request, "pixel", tmp)
            if error:
                problems.append(f"{directory.name}: {error}")
            elif first != second:
                problems.append(f"{directory.name}: not deterministic")
            elif parse_output(first)[3]:
                problems.append(f"{directory.name}: lines outside any block")
            if snapshot(repo) != before:
                problems.append(f"{directory.name}: modified the repository")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("probe", nargs="?")
    parser.add_argument("--questions", default=None, metavar="JSONL",
                        help="question file, with baselines.jsonl beside it (default: eval/brief-bench/questions.jsonl)")
    parser.add_argument("--census-db", default=None, metavar="DB",
                        help="the private census instead of the public bench: its census.db (local only)")
    parser.add_argument("--set", choices=("census", "bench"), default=None, help=argparse.SUPPRESS)  # the old spelling
    parser.add_argument("--bin", default=os.environ.get("PIXEL_BIN") or str(WORKTREE / "target/dev-release/pixel"))
    parser.add_argument("--root", default=None, help="state directory of the census source (default: the directory of "
                        "--census-db, else $PIXEL_CENSUS_ROOT, else $TMPDIR/pixel-tool-census)")
    parser.add_argument("--bench", default=None, help=argparse.SUPPRESS)  # a bench directory; --questions names its file
    parser.add_argument("--cache", default=str(Path.home() / ".cache" / "pixel-brief-bench"))
    parser.add_argument("--census-dir", default=None)
    parser.add_argument("--ids", default=None, help="only these question ids")
    parser.add_argument("--no-history", action="store_true", help="census clones without the history index (the state a history fallback is for)")
    parser.add_argument("--report-dir", default=None)
    parser.add_argument("--guard", default=None, metavar="PROMPT",
                        help="refuse a probe whose code spells out words or paths of this one prompt")
    parser.add_argument("--guard-paths", default="", help="comma-separated evidence paths of that prompt")
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--agent", choices=("codex", "claude"), default=None,
                        help="the live agent (default: claude under Claude Code, else codex; codex for heavy loops)")
    parser.add_argument("--live-jobs", type=int, default=4)
    parser.add_argument("--live-reps", type=int, default=1, help="Codex runs per arm (default 1; use 3 before claiming a 0 on a borderline result); the verdict reads the medians")
    parser.add_argument("--hook-shim", default=None, help=argparse.SUPPRESS)
    parser.add_argument("--check-all", action="store_true",
                        help="contract check of every probe of probes/ on a throwaway repository (CI)")
    parser.add_argument("--self-test", action="store_true")
    args, rest = parser.parse_known_args(argv)
    args.agent = args.agent or load_bc().default_agent()
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    if args.check_all:
        problems = check_all()
        for problem in problems:
            print(problem)
        print(f"probes/: {'ok' if not problems else str(len(problems)) + ' problem(s)'}")
        return 1 if problems else 0
    if args.hook_shim:
        return hook_shim(args.hook_shim, ([args.probe] if args.probe else []) + rest)
    if not args.probe:
        parser.error("a probe directory or name")
    probe = Probe(resolve_probe(args.probe))
    binary = str(Path(args.bin).resolve())
    bc = load_bc()
    sb = bc.load_module("score_brief", args.census_dir)
    qs = bc.sources(args.census_dir)
    if args.set == "census" and not args.census_db:
        args.census_db = str(qs.resolve_root(args) / "census.db")
    if args.bench and not args.questions:
        args.questions = str(Path(args.bench) / "questions.jsonl")
    source = qs.open_source(args)
    sb.add_workspace_root(args.root)
    if source.kind == "census":
        qset = CensusSet(bc, sb, args, binary, source)
    else:
        args.bench = str(source.path.parent)
        qset = BenchSet(bc, sb, args, binary)
    ids = args.ids.split(",") if args.ids else qset.ids()
    home = tempfile.mkdtemp(prefix="probe-home-")
    rows = evaluate(probe, qset, binary, ids, home) if not probe.problems else []
    live = None
    if args.live and not probe.problems:
        targets = [r["id"] for r in rows if r["ran"]]
        live = live_run(probe, qset, binary, targets, args, args.report_dir or tempfile.mkdtemp(prefix="probe-live-"))
    verdict, reasons = verdict_of(probe, rows, live)
    if args.guard:
        hard = overfit(probe.dir, args.guard, [p for p in args.guard_paths.split(",") if p])
        if hard:
            reasons.append("overfitted to the prompt: the code spells out " + ", ".join(f"`{h}`" for h in hard))
            verdict = "FAIL" if live is None else "DROP"
    report_dir = args.report_dir or str(WORKTREE / "target" / "probe-reports" / probe.name)
    write_report(report_dir, probe, rows, verdict, reasons, live)
    print(verdict_line(probe, rows, verdict, live))
    for reason in reasons:
        print(f"  - {reason}")
    return 0 if verdict in ("KEEP", "OK") else 1


class SelfTest(unittest.TestCase):
    def write_probe(self, tmp, name="p", toml_extra="", body="#!/bin/sh\ncat >/dev/null\n"):
        directory = Path(tmp) / name
        directory.mkdir()
        (directory / "pixel-plugin.toml").write_text(
            f'name = "{name}"\nversion = "0.1.0"\napi = 1\ndescription = "d"\nrun = "probe"\n'
            f'capabilities = ["brief-probe"]\n\n[brief-probe]\nowner = "o"\nbudget_ms = 500\nmode = "append"\n{toml_extra}')
        script = directory / "probe"
        script.write_text(body)
        script.chmod(0o755)
        return directory

    def test_check_all_holds_for_the_shipped_probes_and_names_a_broken_one(self):
        self.assertEqual(check_all(), [])
        with tempfile.TemporaryDirectory() as tmp:
            self.write_probe(tmp, "ok", body="#!/bin/sh\ncat >/dev/null\nprintf '### a — b\\nline\\n'\n")
            self.write_probe(tmp, "noisy", body="#!/bin/sh\ncat >/dev/null\necho stray\n")
            self.write_probe(tmp, "flaky", body="#!/bin/sh\ncat >/dev/null\nprintf '### a — %s\\n' $$\n")
            problems = check_all(tmp)
            self.assertTrue(any(p.startswith("noisy: lines outside") for p in problems), problems)
            self.assertTrue(any(p.startswith("flaky: not deterministic") for p in problems), problems)
            self.assertFalse(any(p.startswith("ok:") for p in problems), problems)

    @unittest.skipUnless(shutil.which("rg"), "ripgrep is not installed")
    def test_error_origin_finds_the_raise_site_its_callers_and_a_test_and_keeps_to_the_origins_directory(self):
        probe = Path(__file__).resolve().parent.parent / "probes" / "error-origin"
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            files = {
                "crates/pixel/src/decide.rs": "// header\nfn parse(answer: &str) -> Result<(), String> {\n    if answer.is_empty() {\n"
                                              "        return Err(\"remote decisions refused the decision: (no reason given)\".into());\n    }\n    Ok(())\n}\n"
                                              "\n#[cfg(test)]\nfn helper() {}\n\nfn after_a_lone_cfg_test() { parse(\"\").ok(); }\n",
                "crates/pixel/src/run.rs": "fn run(preset: u8) {\n    let decisions = preset == 3;\n    let x = (decisions, 1);\n    parse(\"a\").ok();\n}\n",
                "crates/pixel/src/main.rs": "fn main() { run(3); }\n",
                "crates/pixel-other/src/lib.rs": "fn run(p: u8) { other(p); }\nfn other(p: u8) { run(p); }\nfn caller_of_run() { run(1); }\n",
                "crates/pixel/tests/decide_test.rs": "fn refuses_an_empty_answer() {\n    parse(\"\").ok();\n}\n",
            }
            for rel, text in files.items():
                (repo / rel).parent.mkdir(parents=True, exist_ok=True)
                (repo / rel).write_text(text)
            request = {"prompt": "where does 'remote decisions refused the decision: (nothing)' come from, what triggers it",
                       "repo_root": str(repo), "brief": "[PIXEL:BRIEF]\n", "kind": None, "session_files": []}
            done = subprocess.run([str(probe / "probe")], input=json.dumps(request), capture_output=True, text=True,
                                  cwd=repo)
            self.assertEqual(done.returncode, 0, done.stderr)
            text = done.stdout
            self.assertIn("### error origin — the quoted message is raised at crates/pixel/src/decide.rs:4 in parse", text)
            self.assertIn("run → parse: the lines leading to the call", text)
            self.assertIn("let decisions = preset == 3;", text, "the lines that select the path")
            self.assertIn("test refuses_an_empty_answer exercising parse", text)
            self.assertIn("after_a_lone_cfg_test", text, "a lone #[cfg(test)] does not make the rest of the file test code")
            self.assertNotIn("pixel-other", text, "crates/pixel/ is not crates/pixel-other/")
            self.assertNotIn("let x = (", text.split("test refuses")[0].split("run → parse")[0], "a Rust `let x = (` is not a JS arrow")
            # A comment that mentions the message is not its raise site; an ambiguous function name only counts callers
            # beside the selected definition; a second quote is tried when the first is raised nowhere.
            (repo / "crates/pixel/src/notes.rs").write_text("// remote decisions refused the decision: see decide.rs\nfn notes() {}\n")
            (repo / "crates/pixel-other/src/dup.rs").write_text("fn parse() {}\nfn uses_the_other_parse() { parse(); }\n")
            second = dict(request, prompt="no 'a message that is nowhere in the code' but 'remote decisions refused the decision: x' yes")
            again = subprocess.run([str(probe / "probe")], input=json.dumps(second), capture_output=True, text=True, cwd=repo).stdout
            self.assertIn("raised at crates/pixel/src/decide.rs:4 in parse", again, "the comment is skipped, the second quote tried")
            self.assertNotIn("uses_the_other_parse", again, "parse is defined twice: callers beside the selected one only")
            self.assertIn("run → parse", again)
            quiet = dict(request, prompt="where is the retry limit defined")
            nothing = subprocess.run([str(probe / "probe")], input=json.dumps(quiet), capture_output=True, text=True, cwd=repo)
            self.assertEqual(nothing.stdout, "", "no quoted message: nothing to add")

    def test_a_valid_probe_has_no_problems_and_each_broken_one_names_its_problem(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = Probe(self.write_probe(tmp))
            self.assertEqual(good.problems, [])
            bad = self.write_probe(tmp, "bad", 'when_kinds = ["nonsense"]\n')
            text = (bad / "pixel-plugin.toml").read_text().replace("500", "5000").replace('"append"', '"middle"')
            (bad / "pixel-plugin.toml").write_text(text.replace("owner", "owners"))
            (bad / "probe").chmod(0o644)
            problems = " | ".join(Probe(bad).problems)
            for needle in ("budget_ms must be", "mode must be", "unknown kind `nonsense`", "not an executable",
                           "owner is required", "unknown key `owners`"):
                self.assertIn(needle, problems)
            other = self.write_probe(tmp, "net")
            (other / "pixel-plugin.toml").write_text((other / "pixel-plugin.toml").read_text().replace(
                'run = "probe"', 'run = "probe"\nnetwork = true'))
            self.assertIn("uses no network", " ".join(Probe(other).problems))

    def test_when_is_a_kind_or_a_prompt_regex_and_always_when_neither_is_given(self):
        with tempfile.TemporaryDirectory() as tmp:
            probe = Probe(self.write_probe(tmp, toml_extra='when_kinds = ["flow"]\nwhen_prompt = "drop"\n'))
            self.assertTrue(probe.applies("flow", "x"))
            self.assertTrue(probe.applies("lookup", "an image drop"))
            self.assertFalse(probe.applies("lookup", "an image"))
            self.assertTrue(Probe(self.write_probe(tmp, "always")).applies("anything", "x"))
            skipping = Probe(self.write_probe(tmp, "skipping", toml_extra='skip_kinds = ["overview"]\n'))
            self.assertEqual(skipping.problems, [])
            self.assertTrue(skipping.applies(None, "x"), "no kind settled: the probe may run")
            self.assertFalse(skipping.applies("overview", "x"))

    BRIEF = ("[PIXEL:BRIEF]\nregion pack — x\n### a.rs:1-2 — alpha\n1| a\n2| b\n### b.rs:1-1 — beta\n1| c\n"
             "### c.rs:1-1 — gamma\n1| d\nAnswer from this context. Use a tool only if something you need is not shown above.")

    def test_split_keeps_the_head_and_the_closing_directive_out_of_the_blocks(self):
        head, blocks, footer = split_brief(self.BRIEF + "\n")
        self.assertEqual(head, ["[PIXEL:BRIEF]", "region pack — x"])
        self.assertEqual([b[0] for b in blocks], ["### a.rs:1-2 — alpha", "### b.rs:1-1 — beta", "### c.rs:1-1 — gamma"])
        self.assertEqual(blocks[2], ["### c.rs:1-1 — gamma", "1| d"])
        self.assertTrue(footer[0].startswith("Answer from this context."))

    def test_merge_applies_drop_then_first_then_places_lead_before_and_append_after(self):
        lead = "### z.rs:1-1 — lead\n1| z\ndrop: beta\nfirst: gamma\n"
        tail = "### y.rs:1-1 — tail\n1| y\n"
        text = merge(self.BRIEF, [("b", "append", tail), ("a", "lead", lead)])
        headers = [l[4:] for l in text.splitlines() if l.startswith("### ")]
        self.assertEqual(headers, ["z.rs:1-1 — lead", "c.rs:1-1 — gamma", "a.rs:1-2 — alpha", "y.rs:1-1 — tail"])
        self.assertTrue(text.rstrip().endswith("not shown above."), "the directive stays last")
        self.assertNotIn("1| c", text, "the dropped block is gone with its lines")

    def test_a_probe_cannot_drop_its_own_blocks_and_may_replace_the_brief_by_dropping_everything(self):
        out = "### a.rs:1-2 — alpha\n1| other\n### n.rs:1-1 — new\n1| n\ndrop: .\n"
        text = merge(self.BRIEF, [("p", "append", out)])
        self.assertIn("### n.rs:1-1 — new", text)
        self.assertIn("1| other", text, "the probe's own alpha survives its drop")
        self.assertNotIn("1| a\n", text, "the brief's alpha was dropped")
        self.assertNotIn("### b.rs", text)
        kept = merge(self.BRIEF, [("p", "append", "### a.rs:1-2 — alpha\n1| other\n")])
        self.assertIn("1| a\n", kept, "a duplicate header is skipped: the brief's block stays")
        self.assertNotIn("1| other", kept)

    def test_over_the_cap_the_append_blocks_go_first_then_the_briefs_last_blocks_whole(self):
        big = "### big.rs:1-1 — big\n" + "".join(f"{i}| {'x' * 50}\n" for i in range(40))
        one = merge(self.BRIEF, [("p", "append", big)], cap=400)
        self.assertNotIn("big.rs", one)
        self.assertIn("### a.rs:1-2 — alpha", one)
        two = merge(self.BRIEF, [("p", "append", big)], cap=130)
        self.assertEqual([l for l in two.splitlines() if l.startswith("### ")], ["### a.rs:1-2 — alpha"])
        self.assertTrue(two.rstrip().endswith("not shown above."))

    def test_the_merge_matches_the_golden_cases_the_native_brief_is_tested_against(self):
        cases = json.loads((WORKTREE / "crates/pixel/tests/fixtures/probe-merge.json").read_text())
        self.assertGreaterEqual(len(cases), 10)
        for case in cases:
            outputs = [(o["name"], o["mode"], o["stdout"]) for o in case["outputs"]]
            self.assertEqual(merge(case["brief"], outputs, case["cap"]), case["expected"], case["name"])

    def test_an_empty_brief_gets_the_tag_and_the_probe_blocks(self):
        text = merge("", [("p", "lead", "### a — b\nline\n")])
        self.assertEqual(text, "[PIXEL:BRIEF]\n### a — b\nline\n")

    def test_stray_lines_are_counted_and_directives_parsed(self):
        blocks, drops, firsts, stray = parse_output("noise\n### h\nbody\ndrop: ^x$\nfirst: y\n")
        self.assertEqual((blocks, drops, firsts, stray), ([["### h", "body"]], ["^x$"], ["y"], 1))

    def test_run_probe_enforces_the_budget_and_reports_failures_instead_of_raising(self):
        with tempfile.TemporaryDirectory() as tmp:
            slow = Probe(self.write_probe(tmp, "slow", body="#!/bin/sh\nsleep 5\n"))
            _, ms, error = run_probe(slow, {"repo_root": tmp}, "pixel", tmp)
            self.assertEqual((error, ms), ("timeout", 500))
            broken = Probe(self.write_probe(tmp, "broken", body="#!/bin/sh\nexit 3\n"))
            self.assertTrue(run_probe(broken, {"repo_root": tmp}, "pixel", tmp)[2].startswith("exit 3"))
            ok = Probe(self.write_probe(tmp, "ok", body="#!/bin/sh\ncat >/dev/null\necho '### a — b'\n"))
            self.assertEqual(run_probe(ok, {"repo_root": tmp}, "pixel", tmp)[0], "### a — b\n")

    def row(self, qid, ran, before, after, **extra):
        base = {"id": qid, "ran": ran, "applies": ran, "error": None, "probe_ms": 10, "deterministic": True,
                "read_only": True, "stray": 0, "before": before, "after": after}
        return {**base, **extra}

    def test_the_offline_verdict_fails_a_probe_that_hurts_and_a_live_one_that_helps_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            probe = Probe(self.write_probe(tmp))
            good = [self.row("a", True, {"calls": 3, "quoted": 0.1, "bytes": 1}, {"calls": 1, "quoted": 0.4, "bytes": 2})]
            self.assertEqual(verdict_of(probe, good)[0], "OK")
            worse = good + [self.row("b", True, {"calls": 0, "quoted": 0.5, "bytes": 1}, {"calls": 2, "quoted": 0.5, "bytes": 2})]
            verdict, reasons = verdict_of(probe, worse)
            self.assertEqual(verdict, "FAIL")
            self.assertTrue(any(r.startswith("b: worse") for r in reasons), reasons)
            nothing = [self.row("a", True, {"calls": 3, "quoted": 0.1, "bytes": 1}, {"calls": 3, "quoted": 0.1, "bytes": 1})]
            self.assertEqual(verdict_of(probe, nothing), ("OK", []), "offline a gain is not required")
            self.assertEqual(verdict_of(probe, [self.row("a", False, {"calls": 1, "quoted": 0, "bytes": 0},
                                                            {"calls": 1, "quoted": 0, "bytes": 0})])[0], "FAIL")
            slow = [self.row("a", True, {"calls": 3, "quoted": 0.1, "bytes": 1}, {"calls": 1, "quoted": 0.4, "bytes": 2},
                             deterministic=False, read_only=False, probe_ms=900)]
            reasons = " ".join(verdict_of(probe, slow)[1])
            self.assertIn("not deterministic", reasons)
            self.assertIn("modified the repository", reasons)

    def test_a_live_arm_that_never_completed_is_a_failure_not_a_drop_in_calls(self):
        with tempfile.TemporaryDirectory() as tmp:
            probe = Probe(self.write_probe(tmp))
            rows = [self.row("a", True, {"calls": 3, "quoted": 0.1, "bytes": 1}, {"calls": 1, "quoted": 0.4, "bytes": 2})]
            verdict, reasons = verdict_of(probe, rows, [{"id": "a", "without": 4, "with": -1, "without_runs": [4],
                                                         "with_runs": []}])
            self.assertEqual(verdict, "DROP")
            self.assertTrue(any("did not complete" in r for r in reasons), reasons)

    def test_a_live_run_that_costs_calls_drops_the_probe(self):
        with tempfile.TemporaryDirectory() as tmp:
            probe = Probe(self.write_probe(tmp))
            rows = [self.row("a", True, {"calls": 3, "quoted": 0.1, "bytes": 1}, {"calls": 1, "quoted": 0.4, "bytes": 2})]
            self.assertEqual(verdict_of(probe, rows, [{"id": "a", "without": 4, "with": 1}])[0], "KEEP")
            self.assertEqual(verdict_of(probe, rows, [{"id": "a", "without": 1, "with": 4}])[0], "DROP")
            self.assertEqual(verdict_of(probe, rows, [{"id": "a", "without": 1, "with": 1}])[0], "DROP")
            line = verdict_line(probe, rows, "KEEP", [{"id": "a", "without": 4, "with": 1}])
            self.assertEqual(line, "PROBE p | a | predicted calls 3 -> 1 | coverage 10% -> 40% | live 4 -> 1 calls | KEEP")

    def test_the_hook_shim_pieces_split_at_line_boundaries_under_the_hook_cap(self):
        text = "".join(f"line {i} " + "x" * 90 + "\n" for i in range(300))
        pieces = split_pieces(text)
        self.assertTrue(all(len(p) <= PART_CHARS for p in pieces))
        self.assertEqual("".join(pieces), text)

    def test_the_bench_scores_quoted_oracle_lines_and_uncovered_baseline_calls(self):
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "questions.jsonl").write_text(json.dumps(
                {"id": "q", "repo": "o/r", "sha": "0" * 40, "prompt": "p", "kind": "lookup",
                 "oracle": [["a.py", 1, 4]]}) + "\n")
            (Path(tmp) / "baselines.jsonl").write_text(json.dumps(
                {"id": "q", "calls": [{"reads": [["a.py", 1, 4]], "commits": []}, {"reads": [["b.py", 1, 3]]}]}) + "\n")
            rows, base = load_bench(tmp)
            self.assertEqual(rows[0]["oracle"], [["a.py", 1, 4]])
            self.assertEqual(len(base["q"]["calls"]), 2)

    def test_the_bench_score_replays_the_baseline_with_the_judge_of_brief_check(self):
        bc = load_bc()
        sb = bc.load_module("score_brief", None)
        with tempfile.TemporaryDirectory() as tmp:
            questions, _sha = bc.sources().fixture_bench(tmp)
            qset = object.__new__(BenchSet)
            qset.bc, qset.sb = bc, sb
            qset.rows, qset.baselines = load_bench(questions.parent)
            q = {"id": "t-01", "clone": str(Path(tmp) / "origin"), "row": qset.rows[0]}
            brief = "[PIXEL:BRIEF]\n### src/lib.py:5-9 — lines\n" + "".join(f"{n}| line_{n} = {n}\n" for n in range(5, 10))
            self.assertEqual(qset.score(q, brief), {"calls": 1, "quoted": 1.0, "bytes": len(brief.encode())})
            self.assertEqual(qset.score(q, "[PIXEL:BRIEF]\n")["calls"], 2)
            self.assertEqual(qset.score(q, "[PIXEL:BRIEF]\n")["quoted"], 0.0)

    def test_a_probe_is_judged_offline_on_the_public_bench_with_no_census(self):
        """The whole offline path on one bench question: pinned clone from the question's url, the fake binary's
        brief, the probe merged, both briefs scored against the recorded baseline. No census, network or agent."""
        import contextlib
        import io
        bc = load_bc()
        with tempfile.TemporaryDirectory() as tmp:
            questions, _sha = bc.sources().fixture_bench(tmp)
            brief = "[PIXEL:BRIEF]\n### src/lib.py:1-2 — head\n1| line_1 = 1\n2| line_2 = 2\n"
            fake = Path(tmp) / "pixel-fake"
            fake.write_text("#!/usr/bin/env python3\nimport json, sys\na = sys.argv[1:]\nif a[:1] == ['brief']:\n"
                            f"    print(json.dumps({{'brief': {brief!r}, 'kind': 'lookup', 'gate': 'open'}}))\n")
            fake.chmod(0o755)
            probe = self.write_probe(tmp, "adds-lines", body="#!/bin/sh\ncat >/dev/null\n"
                                     "printf '### src/lib.py:5-9 — lines\\n5| line_5 = 5\\n6| line_6 = 6\\n7| line_7 = 7\\n"
                                     "8| line_8 = 8\\n9| line_9 = 9\\n'\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = main([str(probe), "--bin", str(fake), "--questions", str(questions), "--cache", str(Path(tmp) / "cache"),
                             "--report-dir", str(Path(tmp) / "report")])
            line = out.getvalue().splitlines()[0]
            self.assertEqual(code, 0, out.getvalue())
            self.assertTrue(line.startswith("PROBE adds-lines | t-01 | predicted calls 2 -> 1 |"), line)
            self.assertTrue(line.endswith("| OK"), line)   # KEEP needs a live run
            self.assertTrue((Path(tmp) / "report" / "report.json").exists())

    def test_a_probe_that_spells_out_the_prompts_words_or_evidence_paths_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = Path(self.write_probe(tmp, "good", body="#!/bin/sh\nrg --files | head\n"))
            prompt = "where does pixel decide how many hook parts a brief is split into"
            self.assertEqual(overfit(good, prompt, ["crates/pixel-install/src/config.rs"]), [])
            bad = Path(self.write_probe(tmp, "bad", body="#!/bin/sh\ncat crates/pixel-install/src/config.rs\n"
                                                         "grep brief_parts src; echo hook parts\n"))
            found = overfit(bad, prompt, ["crates/pixel-install/src/config.rs"])
            self.assertIn("crates/pixel-install/src/config.rs", found)
            self.assertIn("parts", found)
            self.assertIn("config", found)
            self.assertNotIn("how", found)
            # vocabulary another probe already uses is allowed
            other = Path(self.write_probe(tmp, "other", body="#!/bin/sh\n# decide parts\n"))
            self.assertTrue((other / "probe").exists())
            self.assertNotIn("parts", overfit(bad, prompt, [], others=Path(tmp)))

    def test_the_literals_of_a_prompt_are_its_content_words_and_its_paths(self):
        lits = literals_of_prompt("where does pixel decide how many hook parts", ["a/b/config.rs"])
        self.assertIn("decide", lits)
        self.assertIn("a/b/config.rs", lits)
        self.assertIn("config.rs", lits)
        self.assertIn("config", lits)
        self.assertNotIn("where", lits)


if __name__ == "__main__":
    sys.exit(main())
