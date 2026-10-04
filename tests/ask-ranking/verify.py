#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Frozen ask evaluation. Run after building: python3 tests/ask-ranking/verify.py.

Inputs frozen before semantic measurements. This directory is excluded by ask's
normal collector, so queries and expected paths cannot improve their own scores.
One run per query/limit measures wall time, not statistical latency equivalence.
This checks retrieved evidence, not autonomous agent-task outcomes.
"""
import argparse
import hashlib
import json
import os
import shutil
import tempfile
from pathlib import Path
import subprocess
import time

QUERIES = [
    ("I don't want to run the pixel install, how can I do a manual thing?", "docs/manual-setup.md", 3),
    ("how to install pixel manually", "docs/manual-setup.md", 3),
    ("How are deleted and renamed files found in history?", "crates/pixel-facts/src/excavate.rs", 8),
    ("How does the index skip excluded directories?", "crates/pixel-index/src/index.rs", 8),
    ("Where are crashed git publish operations recovered?", "crates/pixel-ops/src/recovery.rs", 8),
]


def snapshot(root, binary, destination):
    """Copy only the eligible corpus and executable; never query a changing tree."""
    corpus = destination / "corpus"
    corpus.mkdir()
    excluded = {"target", "node_modules", ".git", ".pixel", "dist", "build", "vendor",
                ".cache", "assets", "reference", "examples", "tests"}
    extensions = {"rs", "toml", "py", "ts", "tsx", "js", "jsx", "go", "c", "h", "cpp", "hpp",
                  "java", "rb", "sh", "md", "json", "yaml", "yml", "sql", "zig", "swift", "kt", "css"}
    digest = hashlib.sha256()
    copied = 0
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in excluded and not (Path(directory) / d).is_symlink())
        for name in sorted(files):
            source = Path(directory) / name
            if source.is_symlink() or not source.is_file() or name.rsplit(".", 1)[-1].lower() not in extensions:
                continue
            relative = source.relative_to(root)
            data = source.read_bytes()
            target = corpus / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
            digest.update(str(relative).encode() + b"\0" + data + b"\0")
            copied += 1
    candidate = destination / "pixel"
    shutil.copy2(binary, candidate)
    return corpus.resolve(), candidate.resolve(), digest.hexdigest(), copied


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("observed.json"))
    args = parser.parse_args()
    root = args.root.resolve()
    binary = (args.binary or root / "target/debug/pixel").resolve()
    temporary = tempfile.TemporaryDirectory(prefix="pixel-ranking-")
    root, binary, corpus_digest, copied = snapshot(root, binary, Path(temporary.name))
    report = {"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "corpus_sha256": corpus_digest, "corpus_files": copied,
              "isolation": "immutable eligible-source and binary copies in disposable temporary directory",
              "limits": {}, "decision": "retain 8",
              "measurement_limits": "single samples; concurrent host work may affect wall times; no agent-outcome or statistical latency claim"}
    for limit in (8, 3, 5):
        rows = []
        for query, expected, maximum_rank in QUERIES:
            start = time.monotonic()
            completed = subprocess.run([str(binary), "ask", query, str(root),
                "--limit", str(limit), "--max-files", "2000", "--json", "--metrics", "off"],
                capture_output=True, text=True, timeout=120, check=True)
            elapsed = time.monotonic() - start
            result = json.loads(completed.stdout)
            assert not result["coverage"]["degraded"], result["coverage"]
            paths = [str(Path(hit["path"]).relative_to(root)) for hit in result["hits"]]
            rank = paths.index(expected) + 1 if expected in paths else None
            retained = rank is not None and rank <= maximum_rank
            rows.append({"query": query, "required_evidence": expected, "required_max_rank": maximum_rank,
                         "observed_rank": rank, "retained": retained, "elapsed_seconds": elapsed,
                         "paths": paths, "coverage": result["coverage"]})
            print(f"limit={limit} rank={rank} retained={retained} seconds={elapsed:.3f}: {query}", flush=True)
        report["limits"][str(limit)] = rows
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    assert all(row["retained"] for row in report["limits"]["8"]), "corrected eight-result baseline lost required evidence"
    for limit in (3, 5):
        lost = sum(not row["retained"] for row in report["limits"][str(limit)])
        print(f"limit={limit}: {lost} required-evidence losses versus corrected eight-result baseline")
    print(f"Baseline accepted; retain 8. Results: {args.output}")
    temporary.cleanup()


if __name__ == "__main__":
    main()
