#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Compare two scorer sources on identical saved public-bench briefs, without rerunning retrieval."""
import argparse
import contextlib
import fcntl
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

METRICS = ("file_recall", "region_cov", "line_cov", "inline_cov")


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    # Each scorer must bind its own checkout's question_source: that module also owns REPO/provenance.
    sys.path.insert(0, str(path.resolve().parent))
    sys.modules.pop("question_source", None)
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.pop(0)
    return module


def replay(args):
    original = json.loads(args.report.read_text())
    if original["questions"]["kind"] != "bench":
        raise SystemExit("replay supports public bench reports only")
    if digest(args.binary) != original["binary"]["sha256"] or digest(args.questions) != original["questions"]["sha256"]:
        raise SystemExit("binary/questions differ from the frozen report")
    by_prompt = {row["prompt"]: row for row in original["sessions"]}
    if len(by_prompt) != len(original["sessions"]):
        raise SystemExit("ambiguous duplicate prompts")
    inputs = {r["id"]: digest(args.briefs / f"{r['id']}.txt") for r in original["sessions"]}
    reports, codes = [], []
    args.out.mkdir(parents=True, exist_ok=True)
    for side, path in (("before", args.before), ("after", args.after)):
        if (args.out / side).exists():
            raise SystemExit(f"{args.out / side} exists; use a fresh output directory")
        module = load(f"replay_{side}", path)
        if module.verify_probe_home(args.probe_home)["probe_files"] != original["probe_home"]["probe_files"]:
            raise SystemExit("probe files differ from the frozen report")

        def frozen(_info, _clone, prompt, _session, **_kwargs):
            row = by_prompt[prompt]
            return ((args.briefs / f"{row['id']}.txt").read_text(), dict(row["brief_meta"]),
                    row["metrics"]["elapsed_ms"], row["metrics"]["exit"])

        argv = ["--bin", str(args.binary), "--questions", str(args.questions), "--root", str(args.root),
                "--probe-home", str(args.probe_home), "--route", "hook", "--label", side, "--out", str(args.out),
                "--only", ",".join(inputs)]
        with mock.patch.object(module, "run_hook_brief", side_effect=frozen), \
             mock.patch.object(module, "indexed_clone", side_effect=lambda root, info, repo, sha, **kw: root / "cache" / repo), \
             mock.patch.object(module, "stop_daemons", return_value=[]), \
             open(args.out / f"{side}.log", "w") as log, contextlib.redirect_stdout(log):
            codes.append(module.main(argv))
        reports.append(json.loads((args.out / side / "report.json").read_text()))
    if inputs != {qid: digest(args.briefs / f"{qid}.txt") for qid in inputs}:
        raise SystemExit("briefs changed during replay")
    before = {r["id"]: r for r in reports[0]["sessions"]}
    changes = []
    for row in reports[1]["sessions"]:
        a, b = before[row["id"]]["metrics"], row["metrics"]
        delta = {k: b[k] - a[k] if a[k] is not None and b[k] is not None else None for k in METRICS}
        changes.append({"id": row["id"], "category": row["category"],
                        "before": {k: a[k] for k in METRICS}, "after": {k: b[k] for k in METRICS}, "delta": delta})
        print(row["id"], "changed" if any(delta.values()) else "same", json.dumps(delta), flush=True)
    category = {}
    for name in sorted({row["category"] for row in reports[1]["sessions"]}):
        category[name] = [module.aggregate([r for r in report["sessions"] if r["category"] == name])
                          for report in reports]
    result = {"kind": "measurement correction, not retrieval improvement", "input_report_sha256": digest(args.report),
              "brief_hashes": inputs, "scorer_hashes": [digest(args.before), digest(args.after)],
              "protocols": [r.get("protocol", {"name": "brief-oracle", "version": 1}) for r in reports],
              "exit_codes": codes, "overall": [r["means"]["all"] for r in reports],
              "per_question": changes, "per_category": category}
    (args.out / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    (args.out / "comparison.md").write_text(module.render_compare(reports, ["before", "after"]))
    print(f"SUMMARY: {len(changes)} questions; {sum(any(r['delta'].values()) for r in changes)} changed; exits {codes}")
    return max(codes)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("before", "after", "report", "briefs", "binary", "probe-home", "questions", "root", "out"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    with open("/tmp/pixel-benchmark.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return replay(args)


if __name__ == "__main__":
    sys.exit(main())
