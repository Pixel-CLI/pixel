#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Fast per-change loop for the prompt-submit brief (issue #906): no LLM, a few seconds per question.

For one answer-only census question it runs ``pixel brief`` (the
path of the prompt-submit hook, 3 parts) in a pinned clone that is indexed once and then reused, and
REPLAYS the tool calls Codex made on that question in the recorded live runs (the brief arm and the
no-brief arm, from the saved ``codex exec --json`` event logs) against the brief:

    COVERED   the brief already holds what the call fetched: the quoted lines of a file range or of
              the matching lines of a search, the commits of a ``git log``, the changed lines of a
              ``git show``/``git diff``;
    residual  anything else: the call a session would still make. ``predicted residual calls`` is
              their number, and the uncovered targets are listed.

    brief-check.py <question-id> [--bin <pixel>] [--no-build] [--extra-cmd CMD] [--brief-file F]
    brief-check.py --all [--jobs 4]       one line per question, with the last live run's calls
    brief-check.py --self-test

``--extra-cmd`` runs a throwaway prototype (env ``QID``, ``CLONE``, ``PROMPT``, ``BRIEF_FILE``) and
appends its stdout to the brief, so a hypothesis can be replayed before any Rust is written.
``--brief-file`` replays a brief you wrote. The questions, the commands and the outputs are private:
only counts, paths and line ranges are printed. The census runner and scorer (``scripts/tool-census``)
are found like ``brief-zero-calls.py`` finds them. Standard library only; run with ``python3 -I``.
"""

import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parent
DEFAULT_ROOT = Path("/private/tmp/claude-501/census")
CENSUS_DIRS = (
    Path(os.environ.get("PIXEL_CENSUS_DIR", "/nonexistent")),
    HERE / "tool-census",
    Path("/Users/livio/Documents/pixel-wt/census/scripts/tool-census"),
)
NON_CALL_ITEMS = frozenset({"agent_message", "reasoning", "error", "todo_list", "user_message", "plan"})
LINE_THRESHOLD = 0.8     # share of a call's lines the brief must quote for the call to be covered
DIFF_THRESHOLD = 0.7     # share of a diff's changed lines the brief must carry
HISTORY_BUDGET_S = 420
PROTOTYPE_RUNS = ("proto-", "hyp-")


def load_module(name, directory):
    for candidate in ([Path(directory)] if directory else list(CENSUS_DIRS)):
        path = candidate / f"{name}.py"
        if path.exists():
            spec = importlib.util.spec_from_file_location(name, path)
            module = importlib.util.module_from_spec(spec)
            sys.modules[name] = module
            spec.loader.exec_module(module)
            return module
    raise SystemExit(f"{name}.py not found: pass --census-dir or set PIXEL_CENSUS_DIR")


def env_for_brief():
    env = dict(os.environ)
    env.pop("ANTHROPIC_API_KEY", None)
    env.pop("PIXEL_BRIEF", None)
    env["PIXEL_BRIEF_PARTS"] = "3"
    return env


def sh(argv, cwd=None, timeout=900, env=None):
    return subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)


# --------------------------------------------------------------------------- recorded calls

def inner_command(command):
    """The script a ``/bin/zsh -lc "..."`` command item ran."""
    try:
        words = shlex.split(command)
    except ValueError:
        return command
    return words[2] if len(words) >= 3 and words[1] in ("-c", "-lc", "-ic") else command


def codex_calls(lines):
    """``[{cmd, out}]`` of the finished command items of one ``codex exec --json`` stream, in order."""
    calls = []
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        item = rec.get("item") or {}
        if rec.get("type") == "item.completed" and item.get("type") == "command_execution":
            calls.append({"cmd": inner_command(str(item.get("command") or "")),
                          "out": str(item.get("aggregated_output") or "")})
    return calls


def default_agent():
    """The agent the harness drives: ``PIXEL_PROBE_AGENT``, else Claude when this runs under Claude Code (it sets
    ``CLAUDECODE``), else Codex. Claude runs draw on the user's Claude quota; Codex is the unlimited choice for
    heavy loops: pass ``--agent codex``."""
    return os.environ.get("PIXEL_PROBE_AGENT") or ("claude" if os.environ.get("CLAUDECODE") else "codex")


def claude_calls(lines):
    """``[{tool, input, output, is_error}]`` of a ``claude -p --output-format stream-json`` stream: the tool_use
    blocks of the top-level agent paired with their tool_result by id. The shape the census scorer reads."""
    uses, order, results = {}, [], {}
    for line in lines:
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        content = (rec.get("message") or {}).get("content")
        if not isinstance(content, list) or rec.get("parent_tool_use_id"):
            continue
        for part in content:
            if rec.get("type") == "assistant" and part.get("type") == "tool_use" and part.get("id") not in uses:
                uses[part["id"]] = {"tool": part.get("name", "?"), "input": part.get("input") or {}}
                order.append(part["id"])
            elif rec.get("type") == "user" and part.get("type") == "tool_result":
                body = part.get("content")
                if isinstance(body, list):
                    body = "".join(b.get("text", "") for b in body if isinstance(b, dict))
                results[part.get("tool_use_id")] = (str(body or ""), bool(part.get("is_error")))
    return [{**uses[i], "output": results.get(i, ("", False))[0], "is_error": results.get(i, ("", False))[1]}
            for i in order]


def call_command(call):
    """The shell-like text of a call of either agent, for the classifier: Codex's command, Claude's Bash command,
    ``rg <pattern>`` for its Grep tool, the empty string for a Read or a Glob."""
    if "cmd" in call:
        return call["cmd"]
    inp = call.get("input") or {}
    if call.get("tool") == "Bash":
        return str(inp.get("command") or "")
    if call.get("tool") == "Grep":
        return "rg " + str(inp.get("pattern") or "")
    return ""


def call_oracle(sb, repo, call):
    """The scorer's oracle of one call, whichever agent made it: a Codex ``{cmd, out}`` or a Claude
    ``{tool, input, output}`` (Read / Grep / Glob / Bash)."""
    if "cmd" in call:
        oracle = sb.Oracle(repo)
        sb.bash_oracle(oracle, call["cmd"], call["out"])
        return oracle
    return sb.build_oracle(repo, [call])


def recorded_traces(root, qid):
    """``{arm: (run id, calls)}``: the newest recorded stream of each arm of ``qid``. A stream from before the arms
    were named (``<id>.stream.jsonl``) is a brief-arm trace."""
    found = {}
    for run in sorted((Path(root) / "zero-calls").glob("*/raw"), key=lambda p: p.stat().st_mtime):
        if run.parent.name.startswith(PROTOTYPE_RUNS):
            continue  # a prototype's own run is evidence for that prototype, not a recording of the brief's gaps
        for arm in ("brief", "nobrief"):
            for name in (f"{qid}.{arm}.stream.jsonl", f"{qid}.stream.jsonl" if arm == "brief" else None):
                path = run / name if name else None
                if path is not None and path.exists():
                    text = path.read_text(encoding="utf-8", errors="replace")
                    if '"turn.completed"' in text:
                        found[arm] = (run.parent.name, codex_calls(text.splitlines()))
    return found


def live_calls(root, qid):
    """Calls of the brief arm in the newest report that has it: ``(run id, n_calls)`` or ``None``."""
    best = None
    for report in sorted((Path(root) / "zero-calls").glob("*/report.json"), key=lambda p: p.stat().st_mtime):
        if report.parent.name.startswith(PROTOTYPE_RUNS):
            continue
        try:
            rows = json.loads(report.read_text())["rows"]
        except (OSError, ValueError, KeyError):
            continue
        for row in rows:
            if row.get("id") == qid and row.get("arm", "brief") == "brief" and row.get("n_calls", -1) >= 0:
                best = (report.parent.name, row["n_calls"])
    return best


# --------------------------------------------------------------------------- replay

def diff_lines(text):
    """The changed lines of a unified diff, stripped, long enough to mean something."""
    out = []
    for line in text.splitlines():
        if line[:1] in "+-" and not line.startswith(("+++", "---")):
            body = line[1:].strip()
            if len(body) >= 12:
                out.append(body)
    return out


def judge_call(sb, repo, regions, brief_text, call):
    """``(verdict, targets)`` of one recorded call: verdict is ``covered``, ``uncovered``, ``external`` (nothing of
    the repository: a user skill, a package manager) or ``opaque`` (a command whose output is not a read)."""
    oracle = sb.Oracle(repo)
    sb.bash_oracle(oracle, call["cmd"], call["out"])
    targets, ok = [], True
    if oracle.lines:
        by_region = {}
        for region in oracle.regions:
            if region["keys"]:
                by_region[(region["file"], region["lo"], region["hi"])] = (
                    len(region["keys"] & regions.inline), len(region["keys"]))
        have = sum(1 for key in oracle.lines if key in regions.inline)
        share = have / len(oracle.lines)
        if share < LINE_THRESHOLD:
            ok = False
            missed = [(path, lo, hi, got, total) for (path, lo, hi), (got, total) in sorted(by_region.items())
                      if got < total * LINE_THRESHOLD]
            ranges = sorted((m for m in missed if m[4] > 1), key=lambda m: -m[4])
            targets += [f"{path}:{lo}-{hi} ({got}/{total} quoted)" for path, lo, hi, got, total in ranges[:4]]
            hits = [m for m in missed if m[4] == 1]
            if hits:
                targets.append(f"{len(hits)} search-hit line(s), e.g. {hits[0][0]}:{hits[0][1]}")
    for commit in sorted(oracle.commits):
        if not any(h.startswith(commit) or commit.startswith(h[:7]) for h in regions.hex):
            ok = False
            targets.append(f"commit {commit} not in the brief")
    patch = diff_lines(call["out"])
    if patch and re.search(r"\bgit\b.*\b(show|diff)\b", call["cmd"]):
        flat = {line.strip() for line in brief_text.splitlines()}
        got = sum(1 for line in patch if any(line in held for held in flat))
        if got < DIFF_THRESHOLD * len(patch):
            ok = False
            targets.append(f"diff {got}/{len(patch)} changed lines carried")
    for path in sorted(oracle.git_files | oracle.list_files):
        if path not in regions.files:
            ok = False
            targets.append(f"{path} not named")
    if oracle.lines or oracle.commits or oracle.git_files or oracle.list_files or patch:
        return ("covered" if ok else "uncovered"), targets
    if re.search(r"(^|[\s'\"])/(Users|private|tmp|opt)/", call["cmd"]) or re.search(r"\b(yarn|npm|pnpm|bun|pip)\b", call["cmd"]):
        return "external", [shorten(call["cmd"])]
    return "opaque", [shorten(call["cmd"])]


def shorten(text, width=90):
    text = " ".join(text.split())
    return text if len(text) <= width else text[:width] + "..."


def quoted_share(sb, repo, regions, trace):
    """Share of the lines a session without a brief read that the brief quotes (0 when there is no such trace)."""
    if not trace:
        return 0.0
    oracle = sb.Oracle(repo)
    for call in trace[1]:
        sb.bash_oracle(oracle, call["cmd"], call["out"])
    return sum(1 for key in oracle.lines if key in regions.inline) / len(oracle.lines) if oracle.lines else 0.0


def replay(sb, repo, regions, brief_text, traces):
    """Per arm: counts and uncovered targets of the recorded calls against the brief."""
    result = {}
    for arm, (run, calls) in traces.items():
        verdicts = [judge_call(sb, repo, regions, brief_text, call) for call in calls]
        counted = [v for v in verdicts if v[0] != "external"]
        result[arm] = {"run": run, "recorded": len(calls),
                       "residual": sum(1 for v in counted if v[0] != "covered"),
                       "external": len(verdicts) - len(counted),
                       "uncovered": [(i + 1, v[0], v[1]) for i, v in enumerate(verdicts) if v[0] in ("uncovered", "opaque")]}
    return result


# --------------------------------------------------------------------------- clones and brief

def question(root, qid):
    db = sqlite3.connect(f"file:{Path(root) / 'census.db'}?mode=ro", uri=True)
    row = db.execute("SELECT id, repo, sha, prompt FROM sessions WHERE source='census' AND mode='answer' AND ok=1 AND id=?",
                     (qid,)).fetchone()
    db.close()
    if not row:
        raise SystemExit(f"no answer-only census question {qid}")
    return row


def question_ids(root):
    db = sqlite3.connect(f"file:{Path(root) / 'census.db'}?mode=ro", uri=True)
    rows = db.execute("SELECT id FROM sessions WHERE source='census' AND mode='answer' AND ok=1 ORDER BY id").fetchall()
    db.close()
    return [r[0] for r in rows]


def clone_is_sound(dest, sha):
    """The clone holds the repository at ``sha`` at its top level: HEAD is the sha and no tracked file is missing.
    A ``cp -R`` into a directory that already existed nests the repository one level down and fails the second."""
    try:
        head = sh(["git", "rev-parse", "HEAD"], cwd=dest, timeout=60).stdout.strip()
        missing = sh(["git", "ls-files", "-d"], cwd=dest, timeout=120).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return False
    return head == sha and not missing and (dest / ".git").exists()


def checked_clone(root, binary, repo, sha, history=True):
    """A clone of the census repository at ``sha`` that stays between runs: indexed (graph, history, meaning vectors)
    the first time, and only given a daemon of the binary under test afterwards. Creation is serialised by a file
    lock, because several questions of one repository (and several processes) ask for the same clone at once."""
    import fcntl
    dest = Path(root) / "score" / "check" / f"{repo}-{sha[:8]}{'' if history else '-nohist'}"
    dest.parent.mkdir(parents=True, exist_ok=True)
    marker = dest / ".pixel" / "brief-check.json"
    digest = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    env = env_for_brief()
    with open(dest.parent / f".lock-{dest.name}", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        first = not marker.exists() or not clone_is_sound(dest, sha)
        if first:
            shutil.rmtree(dest, ignore_errors=True)
            sh(["cp", "-c", "-R", Path(root) / "cache" / repo, dest], timeout=600)
            shutil.rmtree(dest / ".pixel", ignore_errors=True)
            if not clone_is_sound(dest, sha):
                raise SystemExit(f"{dest}: the clone of {repo} is not at {sha[:8]} with its files at the top level")
        try:
            held = json.loads(marker.read_text()).get("sha256") if marker.exists() else None
        except ValueError:
            held = None
        if held == digest:
            return dest
        sh([binary, "daemon", "stop", "."], cwd=dest, timeout=60, env=env)
        sh([binary, "prepare-repo", "."], cwd=dest, timeout=1800, env=env)
        if first:
            promisor = "promisor" in (dest / ".git" / "config").read_text(errors="replace")
            if history and not promisor:
                try:
                    sh([binary, "build-index", "--history", "."], cwd=dest, timeout=HISTORY_BUDGET_S, env=env)
                except subprocess.TimeoutExpired:
                    pass
            sh([binary, "search-meaning", "how does this code work", "."], cwd=dest, timeout=1800, env=env)
        last, same = None, 0
        for _ in range(14):
            out = sh([binary, "brief", "--metrics", "off", "where is the main entry point of this program defined", "."],
                     cwd=dest, timeout=600, env=env).stdout
            same = same + 1 if out == last else 1
            last = out
            if same >= 3:
                break
            time.sleep(1)
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(json.dumps({"sha256": digest}))
    return dest


def run_brief(binary, clone, prompt, tries=5):
    """``(brief text, meta, elapsed ms)`` of ``pixel brief --json`` in ``clone``. A daemon that has just started answers
    the first briefs empty while it finishes what they ask of it, so an empty brief is asked again, up to ``tries``."""
    for attempt in range(tries):
        started = time.monotonic()
        proc = sh([binary, "brief", "--json", "--metrics", "off", prompt, "."], cwd=clone, timeout=300,
                  env=env_for_brief())
        ms = int((time.monotonic() - started) * 1000)
        try:
            meta = json.loads(proc.stdout)
            text = meta.pop("brief", "") or ""
        except ValueError:
            text, meta = "", {"error": (proc.stdout + proc.stderr)[-300:]}
        if text or attempt == tries - 1:
            return text, meta, ms
        if attempt == 1:
            # Two empty briefs: the daemon may be one that was left half-warm; start a fresh one.
            sh([binary, "daemon", "stop", "."], cwd=clone, timeout=60, env=env_for_brief())
        time.sleep(1)


def build(binary_dir, no_build):
    if no_build:
        return
    env = dict(os.environ, CARGO_TARGET_DIR=str(WORKTREE / "target"), CARGO_PROFILE_DEV_DEBUG="0")
    env.pop("ANTHROPIC_API_KEY", None)
    proc = sh(["cargo", "build", "--profile", "dev-release", "-p", "pixel-cli", "--quiet"], cwd=WORKTREE, env=env,
              timeout=3600)
    if proc.returncode != 0:
        raise SystemExit("build failed:\n" + proc.stderr[-2000:])


def prepare(root, sb, qid, binary, history=True):
    """The pinned question and its reused clone: ``(repo name, prompt, clone)``."""
    _id, repo_name, sha, prompt = question(root, qid)
    return repo_name, prompt, checked_clone(root, binary, repo_name, sha, history)


def score(root, sb, qid, repo_name, clone, text, ms=0, meta=None):
    """The row of one brief ``text``: quoted share, bytes and the replay of the recorded calls."""
    repo = sb.Repo(clone)
    regions = sb.parse_brief(text, repo)
    traces = recorded_traces(root, qid)
    return {"id": qid, "repo": repo_name, "kind": (meta or {}).get("kind"), "bytes": len(text.encode()), "ms": ms,
            "nb_quoted": quoted_share(sb, repo, regions, traces.get("nobrief")),
            "live": live_calls(root, qid), "replay": replay(sb, repo, regions, text, traces), "text": text}


def check(args, sb, qid, binary):
    root = Path(args.root)
    repo_name, prompt, clone = prepare(root, sb, qid, binary)
    if args.brief_file:
        text, meta, ms = Path(args.brief_file).read_text(), {}, 0
    else:
        text, meta, ms = run_brief(binary, clone, prompt)
    if args.extra_cmd:
        brief_file = Path(tempfile.mkdtemp(prefix="bc-")) / "brief.txt"
        brief_file.write_text(text)
        extra = sh(shlex.split(args.extra_cmd), cwd=clone, env=dict(os.environ, QID=qid, CLONE=str(clone), PROMPT=prompt,
                                                                    BRIEF_FILE=str(brief_file)), timeout=120)
        text = apply_extra(text, extra.stdout)
    return score(root, sb, qid, repo_name, clone, text, ms, meta)


REPLACE_MARK = "@@REPLACE@@"


def apply_extra(text, extra):
    """What a prototype script printed, applied to the brief: appended, or replacing it after ``@@REPLACE@@``."""
    if extra.startswith(REPLACE_MARK):
        return extra[len(REPLACE_MARK):].lstrip("\n")
    return text.rstrip("\n") + "\n" + extra if extra.strip() else text


def residual_of(row):
    """Calls a session would still make: the residual of the brief-arm trace when there is one (the calls the agent
    made on top of a brief), else of the no-brief trace."""
    replay = row["replay"]
    pick = replay.get("brief") or replay.get("nobrief")
    return pick["residual"] if pick else None


def summary(row):
    """The counts a later run is compared with: bytes, the residual calls, the share of the no-brief reads quoted."""
    return {"bytes": row["bytes"], "residual": residual_of(row), "nb_quoted": row["nb_quoted"]}


def regressions(old, new):
    """One line per question whose brief shrank by a fifth, needs more calls or quotes less of what a session
    without a brief read."""
    out = []
    for qid, was in sorted(old.items()):
        now = new.get(qid)
        if now is None:
            continue
        if now["bytes"] < 0.8 * was["bytes"]:
            out.append(f"{qid}: brief {was['bytes']} -> {now['bytes']} B")
        if (now["residual"] or 0) > (was["residual"] or 0):
            out.append(f"{qid}: residual calls {was['residual']} -> {now['residual']}")
        if now["nb_quoted"] < was["nb_quoted"] - 0.05:
            out.append(f"{qid}: no-brief reads quoted {was['nb_quoted']:.0%} -> {now['nb_quoted']:.0%}")
    return out


def render_one(row):
    lines = [f"{row['id']}  kind={row['kind']}  brief {row['bytes']} B in {row['ms']} ms  "
             f"quotes {row['nb_quoted']:.0%} of the lines a no-brief session read"]
    for arm, v in sorted(row["replay"].items()):
        lines.append(f"  {arm:<8} recorded {v['recorded']} calls ({v['run']}, {v['external']} external) -> "
                     f"residual {v['residual']}")
    lines.append(f"predicted residual calls = {residual_of(row)}   (live brief arm: "
                 f"{'-' if not row['live'] else row['live'][1]})")
    for arm, v in sorted(row["replay"].items()):
        for index, verdict, targets in v["uncovered"]:
            lines.append(f"  [{arm} call {index}] {verdict}: " + "; ".join(targets[:6]))
    return "\n".join(lines)


def render_table(rows):
    out = ["| question | kind | bytes | ms | quoted | nobrief residual | brief residual | predicted | live brief |",
           "|---|---|---:|---:|---:|---:|---:|---:|---:|"]
    pairs = []
    for row in rows:
        rep = row["replay"]
        nb = rep.get("nobrief", {}).get("residual")
        br = rep.get("brief", {}).get("residual")
        live = row["live"][1] if row["live"] else None
        out.append(f"| {row['id']} | {row['kind']} | {row['bytes']} | {row['ms']} | {row['nb_quoted']:.0%} | "
                   f"{'-' if nb is None else nb} | "
                   f"{'-' if br is None else br} | {residual_of(row)} | {'-' if live is None else live} |")
        if live is not None and residual_of(row) is not None:
            pairs.append((residual_of(row), live))
    if pairs:
        zero_agree = sum(1 for p, a in pairs if (p == 0) == (a == 0))
        mae = sum(abs(p - a) for p, a in pairs) / len(pairs)
        out.append("")
        out.append(f"agreement with the last live run over {len(pairs)} questions: zero/non-zero agrees on "
                   f"{zero_agree}, mean absolute error {mae:.1f} calls")
    out.append(f"predicted zero-call questions: {sum(1 for r in rows if residual_of(r) == 0)} of {len(rows)}")
    return "\n".join(out)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("question", nargs="?")
    parser.add_argument("--bin", default=str(WORKTREE / "target" / "dev-release" / "pixel"))
    parser.add_argument("--root", default=str(DEFAULT_ROOT))
    parser.add_argument("--census-dir", default=None)
    parser.add_argument("--build", action="store_true", help="rebuild with cargo first (default: use --bin as it is)")
    parser.add_argument("--no-build", action="store_true", help=argparse.SUPPRESS)  # the default now; kept for old callers
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--extra-cmd", default=None)
    parser.add_argument("--brief-file", default=None)
    parser.add_argument("--show-brief", action="store_true")
    parser.add_argument("--save-baseline", default=None, help="write the --all rows (counts only) to this JSON file")
    parser.add_argument("--baseline", default=None, help="with --all: flag every question that is worse than this file")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    if not args.question and not args.all:
        parser.error("a question id or --all")
    sb = load_module("score_brief", args.census_dir)
    build(args.bin, not args.build)
    binary = str(Path(args.bin).resolve())
    ids = question_ids(args.root) if args.all else [args.question]
    if args.all:
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
            rows = list(pool.map(lambda q: check(args, sb, q, binary), ids))
        print(render_table(rows))
        if args.save_baseline:
            Path(args.save_baseline).write_text(json.dumps({r["id"]: summary(r) for r in rows}, indent=1) + "\n")
        if args.baseline:
            worse = regressions(json.loads(Path(args.baseline).read_text()), {r["id"]: summary(r) for r in rows})
            print("\nregressions against the baseline: " + ("none" if not worse else ""))
            for line in worse:
                print("  " + line)
            return 1 if worse else 0
    else:
        row = check(args, sb, ids[0], binary)
        print(row["text"] if args.show_brief else render_one(row))
        if args.show_brief:
            print(render_one(row), file=sys.stderr)
    return 0


class SelfTest(unittest.TestCase):
    def test_inner_command_unwraps_the_shell_wrapper(self):
        self.assertEqual(inner_command("/bin/zsh -lc \"sed -n '1,5p' a.rs && rg x .\""), "sed -n '1,5p' a.rs && rg x .")
        self.assertEqual(inner_command("git status"), "git status")

    def test_codex_calls_keep_command_and_output_of_finished_commands_only(self):
        def event(kind, item):
            return json.dumps({"type": kind, "item": item})
        lines = [event("item.started", {"id": "a", "type": "command_execution", "command": "ls"}),
                 event("item.completed", {"id": "a", "type": "command_execution", "command": "/bin/zsh -lc 'ls'",
                                          "aggregated_output": "x\n"}),
                 event("item.completed", {"id": "b", "type": "agent_message", "text": "hi"}), "not json"]
        self.assertEqual(codex_calls(lines), [{"cmd": "ls", "out": "x\n"}])

    def test_diff_lines_keep_long_changed_lines_without_file_headers(self):
        text = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-short\n+the new longer line\n context line here\n"
        self.assertEqual(diff_lines(text), ["the new longer line"])

    def test_the_newest_recorded_stream_of_each_arm_wins_and_old_streams_are_brief_traces(self):
        with tempfile.TemporaryDirectory() as tmp:
            def stream(run, name, command, age):
                path = Path(tmp) / "zero-calls" / run / "raw" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps({"type": "item.completed", "item": {
                    "id": "c", "type": "command_execution", "command": command, "aggregated_output": ""}}) + "\n" + json.dumps({"type": "turn.completed"}) + "\n")
                os.utime(path.parent, (time.time() - age, time.time() - age))
            stream("old", "q-1.stream.jsonl", "old", 300)
            stream("new", "q-1.brief.stream.jsonl", "new", 200)
            stream("new", "q-1.nobrief.stream.jsonl", "base", 200)
            traces = recorded_traces(tmp, "q-1")
            self.assertEqual(traces["brief"][1][0]["cmd"], "new")
            self.assertEqual(traces["nobrief"][1][0]["cmd"], "base")

    def test_live_calls_reads_the_newest_report_row_of_the_brief_arm(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "zero-calls" / "r1"
            run.mkdir(parents=True)
            (run / "report.json").write_text(json.dumps({"rows": [
                {"id": "q-1", "arm": "nobrief", "n_calls": 9}, {"id": "q-1", "arm": "brief", "n_calls": 2}]}))
            self.assertEqual(live_calls(tmp, "q-1"), ("r1", 2))
            self.assertIsNone(live_calls(tmp, "q-2"))

    def test_the_table_reports_how_well_the_prediction_matches_the_live_run(self):
        rows = [{"id": "a", "kind": "k", "bytes": 1, "ms": 2, "live": ("r", 0), "nb_quoted": 0.5,
                 "replay": {"brief": {"residual": 0}, "nobrief": {"residual": 3}}},
                {"id": "b", "kind": "k", "bytes": 1, "ms": 2, "live": ("r", 4), "nb_quoted": 0.0, "replay": {"brief": {"residual": 6}}}]
        text = render_table(rows)
        self.assertIn("| a | k | 1 | 2 | 50% | 3 | 0 | 0 | 0 |", text)
        self.assertIn("zero/non-zero agrees on 2, mean absolute error 1.0 calls", text)
        self.assertIn("predicted zero-call questions: 1 of 2", text)

    def test_a_baseline_flags_a_shrunk_brief_more_calls_and_less_quoted(self):
        old = {"a": {"bytes": 1000, "residual": 0, "nb_quoted": 0.5}, "b": {"bytes": 1000, "residual": 1, "nb_quoted": 0.5}}
        same = {"a": {"bytes": 900, "residual": 0, "nb_quoted": 0.46}, "b": {"bytes": 1500, "residual": 0, "nb_quoted": 0.9}}
        self.assertEqual(regressions(old, same), [])
        worse = {"a": {"bytes": 700, "residual": 2, "nb_quoted": 0.3}, "c": {"bytes": 1, "residual": 9, "nb_quoted": 0.0}}
        self.assertEqual(regressions(old, worse), ["a: brief 1000 -> 700 B", "a: residual calls 0 -> 2",
                                                   "a: no-brief reads quoted 50% -> 30%"])

    def test_claude_calls_pair_tool_use_with_result_and_ignore_subagents(self):
        def line(**rec):
            return json.dumps(rec)
        lines = [
            line(type="assistant", message={"content": [{"type": "tool_use", "id": "a", "name": "Read",
                                                           "input": {"file_path": "/r/a.py", "offset": 3}}]}),
            line(type="user", message={"content": [{"type": "tool_result", "tool_use_id": "a", "content": "x"}]}),
            line(type="assistant", message={"content": [{"type": "tool_use", "id": "b", "name": "Grep",
                                                           "input": {"pattern": "foo"}}]}),
            line(type="user", message={"content": [{"type": "tool_result", "tool_use_id": "b",
                                                      "content": [{"type": "text", "text": "a.py:3:foo"}]}]}),
            line(type="assistant", parent_tool_use_id="z",
                 message={"content": [{"type": "tool_use", "id": "c", "name": "Bash", "input": {}}]}),
            "not json"]
        calls = claude_calls(lines)
        self.assertEqual([c["tool"] for c in calls], ["Read", "Grep"])
        self.assertEqual(calls[1]["output"], "a.py:3:foo")
        self.assertEqual(call_command(calls[1]), "rg foo")
        self.assertEqual(call_command({"cmd": "git log"}), "git log")
        self.assertEqual(call_command({"tool": "Bash", "input": {"command": "ls"}}), "ls")
        self.assertEqual(call_command(calls[0]), "")

    def test_the_default_agent_follows_the_host_unless_overridden(self):
        saved = {k: os.environ.pop(k, None) for k in ("PIXEL_PROBE_AGENT", "CLAUDECODE")}
        try:
            self.assertEqual(default_agent(), "codex")
            os.environ["CLAUDECODE"] = "1"
            self.assertEqual(default_agent(), "claude")
            os.environ["PIXEL_PROBE_AGENT"] = "codex"
            self.assertEqual(default_agent(), "codex")
        finally:
            for key in ("PIXEL_PROBE_AGENT", "CLAUDECODE"):
                os.environ.pop(key, None)
                if saved[key] is not None:
                    os.environ[key] = saved[key]

    def test_a_clone_nested_one_level_down_by_cp_is_not_sound(self):
        def git(cwd, *args):
            return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=cwd,
                                  capture_output=True, text=True, check=True).stdout.strip()
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            git(repo, "init", "-q")
            (repo / "a.txt").write_text("x\n")
            git(repo, "add", ".")
            git(repo, "commit", "-q", "-m", "one")
            sha = git(repo, "rev-parse", "HEAD")
            self.assertTrue(clone_is_sound(repo, sha))
            self.assertFalse(clone_is_sound(repo, "0" * 40), "another commit than the pinned one")
            # `cp -R repo dest` when dest already exists puts the repository at dest/repo: the files are gone from
            # the top level, which is how pixel-4 lost `crates/` and quoted `pixel/crates/...` paths.
            nested = Path(tmp) / "dest"
            nested.mkdir()
            shutil.copytree(repo, nested / "repo")
            shutil.rmtree(nested / ".git", ignore_errors=True)
            shutil.copytree(repo / ".git", nested / ".git")
            self.assertFalse(clone_is_sound(nested, sha))
            (repo / "a.txt").unlink()
            self.assertFalse(clone_is_sound(repo, sha), "a tracked file missing")


if __name__ == "__main__":
    sys.exit(main())
