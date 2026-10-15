#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The probe loop from a plain prompt (issue #906): make "this question" zero-call, reproducibly.

    probe-loop.py --prompt "<question>" [--repo PATH] [--bin PIXEL] [--runs 2] [--max-iter 3]
    probe-loop.py --prompt "<question>" [--repo PATH] --probe NAME [--live-reps 1] [--skip-bench]

First call (no ``--probe``) runs steps 1 to 4 mechanically and stops where a person or Claude must decide:

  1. register the question (``eval/brief-bench/adhoc/``, git-ignored) on the repository at ``--repo`` (default:
     the current repository) pinned at its HEAD, in an indexed clone;
  2. run the CURRENT Pixel brief live (``--bin``, frozen or installed), ``--runs`` times (default 1). If it makes
     0 calls the question is already zero-call: report the answer and the brief size and stop;
  3. only above 0: run the agent with no Pixel hook, ``--runs`` times; the ``path:line`` its answers cite become the
     question's oracle lines;
  4. translate the calls the agent made on top of the brief (what the brief still lacked), then the vanilla calls
     (coverage), into the evidence that would have removed them and say which probe covers each
     (``probe-translate``).

It prints the table and what step 5 must decide: refine the probe that almost covers a call, or scaffold a new one
from ``probes/_template`` as a GENERIC rule (``probe-test.py --guard`` refuses code that spells out words, paths or
symbols of this one prompt). Second call, ``--probe NAME`` (step 6): the overfitting guard, offline ``probe-test``
on the question and, unless ``--skip-bench``, on every bench question (no regression), then live with and without
the probe, and the iteration is logged as hypothesis -> predicted -> live -> kept/dropped. After ``--max-iter``
logged iterations the loop says to stop. The questions, commands and answers stay local (``adhoc/`` is
git-ignored); only counts and paths are printed. Standard library only; run with ``python3 -I``.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parents[1]
BENCH = WORKTREE / "eval" / "brief-bench"
ADHOC = BENCH / "adhoc"


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def git(path, *args):
    return subprocess.run(["git", "-C", str(path), *args], capture_output=True, text=True).stdout.strip()


def question_id(repo_path, sha, prompt):
    return "adhoc-" + hashlib.sha1(f"{repo_path}\n{sha}\n{prompt}".encode()).hexdigest()[:8]


def register(prompt, repo):
    """The ad-hoc question row for ``prompt`` on the repository at ``repo`` (its toplevel, at HEAD), stored."""
    top = git(repo or ".", "rev-parse", "--show-toplevel")
    if not top:
        raise SystemExit(f"{repo or '.'} is not inside a git repository")
    sha = git(top, "rev-parse", "HEAD")
    qid = question_id(top, sha, prompt)
    row = {"id": qid, "repo": f"local/{Path(top).name}", "url": top, "sha": sha, "prompt": prompt, "kind": "adhoc",
           "oracle": [], "oracle_lines": 0}
    ADHOC.mkdir(parents=True, exist_ok=True)
    rows = [r for r in read_jsonl(ADHOC / "questions.jsonl") if r["id"] != qid]
    path = ADHOC / "questions.jsonl"
    old = {r["id"]: r for r in read_jsonl(path)}.get(qid)
    row = {**row, **{k: old[k] for k in ("oracle", "oracle_lines") if old}} if old else row
    path.write_text("".join(json.dumps(r) + "\n" for r in rows + [row]))
    return row


def read_jsonl(path):
    path = Path(path)
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def state_path(qid):
    return ADHOC / qid / "state.json"


def load_state(qid):
    try:
        return json.loads(state_path(qid).read_text())
    except (OSError, ValueError):
        return {"iterations": []}


def save_state(qid, state):
    state_path(qid).parent.mkdir(parents=True, exist_ok=True)
    state_path(qid).write_text(json.dumps(state, indent=1) + "\n")


def decisions(rows):
    """What step 5 must decide, one line per call that no probe covers: refine, or scaffold a generic probe."""
    out = []
    for r in rows:
        if r["status"] == "yes":
            continue
        if r["probes"]:
            out.append(f"- {r['arm'].split()[0]} #{r['n']} ({r['kind']}: {r['target']}): covered by {', '.join(r['probes'])}; "
                       "the brief alone lacks it. Nothing to write unless that probe is gated off for this question.")
        elif r["status"].startswith(("partly", "buried")):
            out.append(f"- {r['arm'].split()[0]} #{r['n']} ({r['kind']}: {r['target']}): {r['status']}. A buried or partial "
                       "answer: refine rank-blocks or the probe that emits the nearest block (drop:/first:, more context).")
        else:
            out.append(f"- {r['arm'].split()[0]} #{r['n']} ({r['kind']}: {r['target']}): not in the brief, no probe. "
                       f"Candidate: a generic `{r['kind']}` probe derived from the prompt and the repository at run time.")
    return out or ["- every vanilla call is already in the brief or covered by a probe: nothing to write."]


def first_pass(args, bb, bc, pt, tr, sb, qset, row, state):
    """Steps 2 to 3: the current brief live first, and only above 0 calls plain Codex. Returns ``(traces, answer)``
    where ``traces`` is empty when the question is already zero-call."""
    clone = qset.clone(row)
    ns = argparse.Namespace(jobs=max(2, 2 * args.runs), timeout=900, model="gpt-5.6-terra", effort="medium",
                            root=args.root, census_dir=args.census_dir, agent=args.agent, claude_model=None,
                            claude_effort=None)
    repo = sb.Repo(clone)
    src = lambda _q: clone  # noqa: E731 - the indexed clone is the source of every session's copy
    withbrief = bb.run_codex(ns, [row], args.runs, binary=args.bin, src=src, tag="adhoc-brief")
    brief_runs = [rec for _q, _r, rec in withbrief if rec]
    if not brief_runs:
        raise SystemExit("no Codex run with the brief completed")
    state["brief"] = {"n_calls": [rec["n_calls"] for rec in brief_runs], "bytes": [rec["brief_bytes"] for rec in brief_runs]}
    if all(rec["n_calls"] == 0 for rec in brief_runs):
        return [], brief_runs[0]["answer"]
    vanilla = bb.run_codex(ns, [row], args.runs, binary=None, src=src, tag="adhoc-plain")
    runs = [rec for _q, _r, rec in vanilla if rec]
    if not runs:
        raise SystemExit("no plain Codex run completed")
    oracle = sorted({tuple(x) for rec in runs for x in bb.oracle_of(sb, repo, rec)})
    row["oracle"] = [list(x) for x in oracle]
    row["oracle_lines"] = sum(hi - lo + 1 for _p, lo, hi in row["oracle"])
    questions = [r for r in read_jsonl(ADHOC / "questions.jsonl") if r["id"] != row["id"]] + [row]
    (ADHOC / "questions.jsonl").write_text("".join(json.dumps(r) + "\n" for r in questions))
    state["vanilla"] = {"n_calls": [rec["n_calls"] for rec in runs],
                        "answers_cite": sum(1 for rec in runs if sb.cited_lines(rec["answer"], repo))}
    # What the brief still lacked first (the calls the agent made on top of it), then the vanilla calls for coverage.
    traces = [(f"brief run {i + 1}", rec["calls"]) for i, rec in enumerate(brief_runs)]
    traces += [(f"vanilla run {i + 1}", rec["calls"]) for i, rec in enumerate(runs)]
    (ADHOC / row["id"]).mkdir(parents=True, exist_ok=True)
    (ADHOC / row["id"] / "traces.json").write_text(json.dumps([[label, calls] for label, calls in traces]))
    return traces, ""


def translate_table(args, bc, pt, tr, sb, qset, row, traces):
    clone = qset.clone(row)
    text, meta, _ = bc.run_brief(args.bin, clone, row["prompt"], tries=3)
    rows = tr.translate(bc, sb, pt, sb.Repo(clone), clone, row["prompt"], text, meta, traces, args.bin)
    return text, meta, rows


def report_first(row, state, text, meta, rows, tr):
    v, b = state["vanilla"]["n_calls"], state["brief"]["n_calls"]
    print(f"# {row['id']}: {row['prompt']}\n")
    print(f"repo {row['url']} at {row['sha'][:8]}; brief {len(text.encode())} B, kind {meta.get('kind')}, "
          f"oracle {row['oracle_lines']} lines (cited by the answers)")
    print(f"vanilla Codex calls {v}; with the current brief {b}\n")
    print(tr.table(rows))
    print("\n" + tr.summary_line(rows))
    print("\n## Step 5, for Claude to decide\n")
    print("\n".join(decisions(rows)))
    print("\nWrite or refine a GENERIC probe (a rule that derives its words, paths and symbols from the prompt and the "
          "repository at run time). `cp -R probes/_template probes/<name>` for a new one. Then:\n"
          f"  python3 -I scripts/bench/probe-loop.py --prompt {json.dumps(row['prompt'])} --repo {row['url']} --probe <name>")


def validate(args, bb, bc, pt, tr, sb, qset, row, state):
    probe = pt.Probe(pt.resolve_probe(args.probe))
    guard = pt.overfit(probe.dir, row["prompt"], sorted({p for p, _lo, _hi in row["oracle"]}))
    ids_bench = [] if args.skip_bench else [r["id"] for r in read_jsonl(BENCH / "questions.jsonl")]
    home = tempfile.mkdtemp(prefix="loop-home-")
    entry = {"probe": probe.name, "hypothesis": args.hypothesis or "(not stated)", "guard": guard}
    rows = pt.evaluate(probe, qset, args.bin, [row["id"]], home) if not probe.problems else []
    neighbours = []
    if not args.skip_bench and not probe.problems:
        bench = pt.BenchSet(bc, sb, argparse.Namespace(bench=str(BENCH), cache=args.cache), args.bin)
        neighbours = pt.evaluate(probe, bench, args.bin, ids_bench, home)
    offline, reasons = pt.verdict_of(probe, rows + neighbours)
    if guard:
        reasons.append("overfitted to the prompt: the code spells out " + ", ".join(f"`{g}`" for g in guard))
        offline = "FAIL"
    target = rows[0] if rows else None
    entry["predicted"] = None if not target else [target["before"]["calls"], target["after"]["calls"]]
    entry["offline"] = offline
    entry["reasons"] = reasons
    live = None
    if offline != "FAIL" and target and target["ran"]:
        ns = argparse.Namespace(live_reps=args.live_reps, live_jobs=max(2, 2 * args.live_reps), root=args.root,
                                census_dir=args.census_dir, agent=args.agent)
        live = pt.live_run(probe, qset, args.bin, [row["id"]], ns, str(ADHOC / row["id"] / "live"))
        verdict, live_reasons = pt.verdict_of(probe, rows, live)
        entry["live"] = live
        entry["verdict"] = "KEEP" if verdict == "KEEP" and not guard else "DROP"
        entry["reasons"] = reasons + [r for r in live_reasons if r not in reasons]
    else:
        entry["verdict"] = "DROP"
        if not (target and target["ran"]):
            entry["reasons"].append("the probe added nothing to this question")
    state["iterations"].append(entry)
    return entry, rows, neighbours


def report_validation(args, row, state, entry, rows):
    n = len(state["iterations"])
    live = entry.get("live")
    after = "-" if not live else f"{live[0]['without']} -> {live[0]['with']}"
    print(f"iteration {n}/{args.max_iter}: hypothesis `{entry['hypothesis']}` -> predicted "
          f"{'-' if not entry['predicted'] else '%s -> %s' % tuple(entry['predicted'])} -> live {after} -> "
          f"{'KEPT' if entry['verdict'] == 'KEEP' else 'DROPPED'} ({entry['probe']})")
    for reason in entry["reasons"]:
        print(f"  - {reason}")
    if entry["verdict"] == "KEEP":
        print("\nKEEP. Show the user the probe diff and the numbers above, then (with their ok) commit the probe.")
    elif n >= args.max_iter:
        print(f"\nStop: {n} iterations logged (--max-iter {args.max_iter}). Report what was tried and the remaining calls.")
    else:
        print("\nNot kept. Translate the calls that remain (re-run without --probe, or `probe-translate`) and iterate.")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--prompt")
    parser.add_argument("--repo", default=None)
    parser.add_argument("--bin", default=os.environ.get("PIXEL_BIN") or str(WORKTREE / "target/dev-release/pixel"))
    parser.add_argument("--root", default=None, help="state directory (default: $PIXEL_CENSUS_ROOT, else $TMPDIR/pixel-tool-census)")
    parser.add_argument("--cache", default=str(Path.home() / ".cache" / "pixel-brief-bench"))
    parser.add_argument("--census-dir", default=None, help="scripts/tool-census (default: PIXEL_CENSUS_DIR, else beside this script)")
    parser.add_argument("--runs", type=int, default=1, help="Codex runs of each first-pass arm")
    parser.add_argument("--max-iter", type=int, default=3)
    parser.add_argument("--agent", choices=("codex", "claude"), default=None,
                        help="the live agent (default: claude under Claude Code, else codex; Claude runs draw on "
                             "the Claude quota, so pass codex for heavy loops)")
    parser.add_argument("--probe", default=None)
    parser.add_argument("--hypothesis", default=None, help="one line, logged with the iteration")
    parser.add_argument("--live-reps", type=int, default=1)
    parser.add_argument("--skip-bench", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    if not args.prompt:
        parser.error("--prompt")
    args.bin = str(Path(args.bin).resolve())
    bb, bc = load("brief_bench", "brief-bench.py"), load("brief_check", "brief-check.py")
    pt, tr = load("probe_test", "probe-test.py"), load("probe_translate", "probe-translate.py")
    sb = bc.load_module("score_brief", args.census_dir)
    args.root = args.root or str(bc.sources(args.census_dir).default_root())
    sb.add_workspace_root(args.root)
    args.agent = args.agent or bc.default_agent()
    row = register(args.prompt, args.repo)
    state = load_state(row["id"])
    qset = pt.BenchSet(bc, sb, argparse.Namespace(bench=str(ADHOC), cache=args.cache), args.bin)
    if args.probe:
        entry, rows, _ = validate(args, bb, bc, pt, tr, sb, qset, row, state)
        save_state(row["id"], state)
        report_validation(args, row, state, entry, rows)
        return 0 if entry["verdict"] == "KEEP" else 1
    traces, answer = first_pass(args, bb, bc, pt, tr, sb, qset, row, state)
    if not traces:
        save_state(row["id"], state)
        print(f"# {row['id']}: already zero-call\n\nprompt: {row['prompt']}\nthe current brief ({args.bin}) made "
              f"{state['brief']['n_calls']} calls, brief bytes {state['brief']['bytes']}.\n\nanswer:\n{answer}\n\n"
              "Nothing to fix: no probe is needed.")
        return 0
    text, meta, rows = translate_table(args, bc, pt, tr, sb, qset, row, traces)
    save_state(row["id"], state)
    report_first(row, state, text, meta, rows, tr)
    return 0


class SelfTest(unittest.TestCase):
    def test_the_question_id_is_stable_and_depends_on_repo_commit_and_prompt(self):
        a = question_id("/r", "1" * 40, "where is x")
        self.assertEqual(a, question_id("/r", "1" * 40, "where is x"))
        self.assertNotEqual(a, question_id("/r", "2" * 40, "where is x"))
        self.assertNotEqual(a, question_id("/r", "1" * 40, "where is y"))
        self.assertNotEqual(a, question_id("/s", "1" * 40, "where is x"))
        self.assertTrue(a.startswith("adhoc-") and len(a) == 14)

    def test_the_decision_lists_each_open_call_with_what_to_do_about_it(self):
        rows = [{"n": 1, "arm": "vanilla run 1", "kind": "doc", "target": "README.md:50-110", "status": "no", "probes": []},
                {"n": 2, "arm": "vanilla run 1", "kind": "region", "target": "a.rs:1-9", "status": "buried at line 145", "probes": []},
                {"n": 3, "arm": "vanilla run 1", "kind": "history", "target": "commits abc", "status": "no", "probes": ["history-fallback"]},
                {"n": 4, "arm": "vanilla run 1", "kind": "region", "target": "b.rs:1-2", "status": "yes", "probes": []}]
        text = "\n".join(decisions(rows))
        self.assertIn("a generic `doc` probe derived from the prompt and the repository at run time", text)
        self.assertIn("refine rank-blocks", text)
        self.assertIn("covered by history-fallback", text)
        self.assertNotIn("b.rs", text)
        self.assertIn("nothing to write", "\n".join(decisions([rows[3]])))

    def test_state_round_trips_and_a_missing_state_starts_empty(self):
        global ADHOC
        old = ADHOC
        with tempfile.TemporaryDirectory() as tmp:
            ADHOC = Path(tmp)
            try:
                self.assertEqual(load_state("adhoc-x"), {"iterations": []})
                save_state("adhoc-x", {"iterations": [{"probe": "p"}]})
                self.assertEqual(load_state("adhoc-x")["iterations"][0]["probe"], "p")
            finally:
                ADHOC = old


if __name__ == "__main__":
    sys.exit(main())
