#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Repeat the integration candidate with the frozen v4 scorer contract, without a live agent."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

SOURCE = "dbff5b09d35a00a07572751d556b330a9da38acd"
SCORER_HASH = "95bf349407a86091b7627f0d7726697478408b743345b32d4ed919da36233106"
QUESTION_HASH = "984a632e96f5f32891279d58f872c3a891bfeabaee110b3993be313c63e17069"


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def git(source, *args):
    return subprocess.check_output(["git", "-C", str(source), *args], text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "binary", "home", "state", "out"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    scorer = args.source / "scripts/bench/tool-census/score_brief.py"
    questions = args.source / "eval/brief-bench/questions.jsonl"
    assert git(args.source, "rev-parse", "HEAD") == SOURCE
    assert not git(args.source, "status", "--porcelain", "--untracked-files=no")
    assert digest(scorer) == SCORER_HASH and digest(questions) == QUESTION_HASH
    version = subprocess.check_output([str(args.binary), "--version"], text=True)
    assert "commit: " + SOURCE + "\n" in version, version
    sys.path.insert(0, str(scorer.parent))
    spec = importlib.util.spec_from_file_location("integration_scorer", scorer)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    probes = module.verify_probe_home(args.home)
    assert probes["commit"] == SOURCE and len(probes["probe_files"]) == 28
    args.out.mkdir(parents=True, exist_ok=False)
    before = dict(source=SOURCE, binary_sha256=digest(args.binary), version=version,
                  scorer_sha256=digest(scorer), question_sha256=digest(questions), probes=probes)
    (args.out / "inputs-before.json").write_text(json.dumps(before, indent=2) + "\n")
    argv = [sys.executable, "-I", str(scorer), "pinned", "--base", str(args.binary),
            "--head", str(args.binary), "--probe-home", str(args.home), "--questions", str(questions),
            "--route", "hook", "--root", str(args.state), "--out", str(args.out), "--label", "integration"]
    print("Waiting for /tmp/pixel-benchmark.lock", flush=True)
    with open("/tmp/pixel-benchmark.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        print("Shared lock acquired; executing both 56-question arms sequentially", flush=True)
        with (args.out / "native.log").open("w") as log:
            code = subprocess.run(argv, cwd=args.source, stdout=log, stderr=subprocess.STDOUT).returncode
    (args.out / "command.json").write_text(json.dumps(dict(argv=argv, exit=code), indent=2) + "\n")
    assert digest(args.binary) == before["binary_sha256"] and digest(scorer) == before["scorer_sha256"]
    assert module.verify_probe_home(args.home) == probes
    for side in ("base", "head"):
        report = json.loads((args.out / ("integration-" + side) / "report.json").read_text())
        print(side, json.dumps(report["result"], sort_keys=True), flush=True)
    print("SUMMARY native pair exit", code, flush=True)
    return code


if __name__ == "__main__":
    sys.exit(main())
