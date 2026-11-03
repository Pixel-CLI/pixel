#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Run the Callgrind suite, reject incomplete evidence, and prove the gate."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

CASES = {
    "wire_read/short", "wire_read/long", "wire_write/short", "wire_write/long",
    "cosine/embedding", "cosine/tail", "extraction/sparse",
}
LIMIT = 5  # Percent Ir growth: a coarse tripwire, not a speedup claim.


def validate(counts):
    if set(counts) != CASES:
        raise ValueError(f"incomplete case set: {set(counts) ^ CASES}")
    if any(type(value) is not int or value <= 0 for value in counts.values()):
        raise ValueError("instruction counts must be positive integers")
    return counts


def compare(before, after, limit=LIMIT):
    validate(before)
    validate(after)
    failures = []
    for case in sorted(CASES):
        delta = (after[case] / before[case] - 1) * 100
        failed = after[case] * 100 > before[case] * (100 + limit)
        print(f"{'FAIL' if failed else 'PASS'} {case}: "
              f"{before[case]} -> {after[case]} Ir ({delta:+.3f}%)")
        if failed:
            failures.append(case)
    return failures


def self_test():
    before = dict.fromkeys(CASES, 1000)
    assert not compare(before, dict.fromkeys(CASES, 1050))
    assert set(compare(before, dict.fromkeys(CASES, 1051))) == CASES
    # One expensive path must not disappear inside an improved aggregate.
    mixed = dict.fromkeys(CASES, 100)
    mixed["cosine/tail"] = 1051
    assert compare(before, mixed) == ["cosine/tail"]
    for bad in ({}, {**before, "extra": 100}, {**before, "cosine/tail": 0},
                {**before, "cosine/tail": True}):
        try:
            compare(before, bad)
        except ValueError:
            continue
        raise AssertionError("missing/invalid evidence passed")
    # The negative control relies on exit 1 meaning regression, not bad input.
    with tempfile.TemporaryDirectory() as directory:
        base = Path(directory) / "base.json"
        candidate = Path(directory) / "candidate.json"
        base.write_text(json.dumps(before))
        for counts, expected in ((before, 0), (mixed, 1), ({}, 2)):
            candidate.write_text(json.dumps(counts))
            result = subprocess.run(
                [sys.executable, "-I", __file__, "--compare", str(base), str(candidate)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
            assert result.returncode == expected, (result.returncode, expected)
    print("PASS checker: threshold, per-case regression, incomplete/invalid evidence")


def command(args, cwd, log, env=None):
    print(f"+ [{cwd}] {' '.join(map(str, args))}", flush=True)
    with log.open("w") as output:
        result = subprocess.run(args, cwd=cwd, env=env, stdout=output,
                                stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        print(log.read_text()[-12000:])
        raise RuntimeError(f"command failed ({result.returncode}); see {log}")
    return log.read_text()


def build(root, out, label):
    # Separate targets prevent cross-checkout timestamp/cache contamination.
    env = {
        **os.environ,
        "CARGO_TARGET_DIR": str(out / f"target-{label}"),
        # Callgrind toggles collection at a named benchmark function. Stripping
        # that symbol yields a successful Valgrind process with zero counts.
        "CARGO_PROFILE_BENCH_STRIP": "none",
        "CARGO_PROFILE_BENCH_DEBUG": "line-tables-only",
    }
    text = command(
        ["cargo", "bench", "--locked", "--no-default-features", "-p", "pixel-bench", "--bench",
         "instructions", "--features", "instruction-counts", "--no-run",
         "--message-format=json"], root, out / f"build-{label}.log", env)
    executables = []
    for line in text.splitlines():
        if line.startswith("{"):
            record = json.loads(line)
            if record.get("target", {}).get("name") == "instructions" and record.get("executable"):
                executables.append(record["executable"])
    if len(executables) != 1:
        raise ValueError(f"expected one benchmark executable, got {executables}")
    return executables[0]


def measure(executable, root, out, label, repetitions=1):
    home = out / label
    home.mkdir()  # Refuse stale results, including a reused output directory.
    env = {**os.environ, "GUNGRAUN_HOME": str(home),
           "PIXEL_BENCH_REPETITIONS": str(repetitions)}
    command([executable, "--bench", "--save-summary=json"], root,
            out / f"{label}.log", env)
    counts = {}
    for path in home.rglob("summary.json"):
        record = json.loads(path.read_text())
        if record["version"] != "7":
            raise ValueError("unsupported Gungraun summary schema")
        case = f"{record['function_name']}/{record['id']}"
        if case in counts:
            raise ValueError(f"duplicate case: {case}")
        profiles = [p for p in record["profiles"] if p["tool"] == "Callgrind"]
        if len(profiles) != 1:
            raise ValueError(f"expected one Callgrind profile: {case}")
        # Each phase has a fresh home: only new values, never stale comparisons.
        values = profiles[0]["data"]["total"]["metrics"]["Ir"]["values"]
        if set(values) != {"new"}:
            raise ValueError(f"unexpected stale comparison: {case}")
        counts[case] = values["new"]
    validate(counts)
    (out / f"{label}.json").write_text(json.dumps(counts, indent=2) + "\n")
    return counts


def run(base, head, out):
    out.mkdir(parents=True, exist_ok=False)
    harness = Path("crates/pixel-bench/benches/instructions.rs")
    if (base / harness).exists() and (base / harness).read_bytes() != (head / harness).read_bytes():
        raise ValueError("benchmark inputs differ; review the harness migration before comparing")
    identity = {
        "base": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=base, text=True).strip(),
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=head, text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "valgrind": subprocess.check_output(["valgrind", "--version"], text=True).strip(),
        "profile": "bench (release inheritance), strip=none, debug=line-tables-only",
        "rustflags": os.environ.get("RUSTFLAGS", ""),
        "limit_percent": LIMIT,
        "gungraun": "0.20.0",
        "harness_sha256": hashlib.sha256((head / harness).read_bytes()).hexdigest(),
        "bootstrap": not (base / harness).exists(),
    }
    (out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    print(json.dumps(identity, indent=2), flush=True)
    head_exe = build(head, out, "head")
    current = measure(head_exe, head, out, "head")
    # Repeated unchanged runs must stay within 1% in both directions.
    for label in ("repeat-1", "repeat-2"):
        repeat = measure(head_exe, head, out, label)
        if compare(current, repeat, 1) or compare(repeat, current, 1):
            raise RuntimeError("unstable unchanged benchmark")
    slower = measure(head_exe, head, out, "deliberate-regression", repetitions=2)
    proof = subprocess.run(
        [sys.executable, "-I", __file__, "--compare", str(out / "head.json"),
         str(out / "deliberate-regression.json")], check=False)
    if proof.returncode != 1 or set(compare(current, slower)) != CASES:
        raise RuntimeError("the gate did not reject every deliberate regression")
    print("PASS negative control: doubled production work rejected by the gate", flush=True)
    if identity["bootstrap"]:
        print("BOOTSTRAP: base has no harness; no base/head performance claim.")
    else:
        baseline = measure(build(base, out, "base"), base, out, "base")
        if compare(baseline, current):
            raise RuntimeError("instruction regression against PR base")
    print("PASS instruction-count lane", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--compare", nargs=2, type=Path)
    parser.add_argument("--base", type=Path)
    parser.add_argument("--head", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
        elif args.compare:
            sys.exit(bool(compare(*(json.loads(p.read_text()) for p in args.compare))))
        elif args.base and args.head and args.out:
            run(args.base.resolve(), args.head.resolve(), args.out.resolve())
        else:
            parser.error("choose --self-test, --compare, or --base/--head/--out")
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
