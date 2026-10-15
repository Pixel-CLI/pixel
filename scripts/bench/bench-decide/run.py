#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Run the decide bake-off: every adapter over the frozen eval sets.

Usage:
  run.py --models gavel,verdict [--sets coding,routing] [--out <dir>]

Writes <out>/<model>.raw.jsonl — one line per spec:
  {id, set, family, qtype, expected, labels, probs, predicted, ms, ok, error?}
Plus <out>/manifest.json with run identity: script, models.py and set
hashes, platform, and per model its load time, ok/error counts and
`runner_peak_rss_bytes`. That RSS is the runner process's cumulative peak
(`getrusage(RUSAGE_SELF)` after the model's last spec): it includes every
model loaded earlier in the same run and excludes child processes, so for
`remote-cli` it does not count the `pixel classify` child at all. Run one
model per process for a per-model figure. Protocol:
docs/bench/decide-bakeoff-protocol.md.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import resource
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE.parent / "fixtures"
sys.path.insert(0, str(HERE))
import models as M  # noqa: E402

SETS = {
    "coding": FIXTURES / "decide-coding-subset.jsonl",
    "routing": FIXTURES / "decide-plan-routing.jsonl",
}


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def load_specs(path):
    return [json.loads(l) for l in Path(path).read_text().splitlines() if l.strip()]


# `ru_maxrss` is in bytes on macOS and in KiB on Linux (getrusage(2)).
RU_MAXRSS_UNIT = {"darwin": 1, "linux": 1024}


def runner_peak_rss_bytes():
    """The runner's cumulative peak RSS in bytes; None where the unit is unknown."""
    unit = RU_MAXRSS_UNIT.get(sys.platform)
    if unit is None:
        return None
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * unit


def predicted(probs, labels):
    return max(labels, key=lambda l: (probs.get(l, 0.0), -labels.index(l)))


def run_model(name, adapter, specs, out_dir):
    t0 = time.perf_counter()
    adapter.load()
    load_s = time.perf_counter() - t0
    raw_path = out_dir / f"{name}.raw.jsonl"
    n_ok = n_err = 0
    with raw_path.open("x") as stream:
        for spec in specs:
            t = time.perf_counter()
            try:
                probs = adapter.decide(spec)
                ms = (time.perf_counter() - t) * 1000
                probs = {l: float(probs.get(l, 0.0)) for l in spec["labels"]}
                rec = {"id": spec["id"], "set": spec["set"], "family": spec["family"],
                       "qtype": spec["qtype"], "expected": spec["expected"],
                       "labels": spec["labels"], "probs": probs,
                       "predicted": predicted(probs, spec["labels"]),
                       "ms": ms, "ok": True}
                n_ok += 1
            except Exception as e:  # noqa: BLE001 - a failed call is a failed attempt
                rec = {"id": spec["id"], "set": spec["set"], "family": spec["family"],
                       "qtype": spec["qtype"], "expected": spec["expected"],
                       "labels": spec["labels"], "probs": None, "predicted": None,
                       "ms": (time.perf_counter() - t) * 1000, "ok": False,
                       "error": f"{type(e).__name__}: {str(e)[:300]}"}
                n_err += 1
            stream.write(json.dumps(rec, ensure_ascii=False) + "\n")
            stream.flush()
    return {"raw": raw_path.name, "load_s": load_s, "n_ok": n_ok, "n_err": n_err,
            "runner_peak_rss_bytes": runner_peak_rss_bytes()}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--models", required=True, help="comma list from: " + ",".join(M.ADAPTERS))
    ap.add_argument("--sets", default="coding,routing")
    ap.add_argument("--out", required=True)
    ap.add_argument("--pixel-bin", default="pixel")
    ap.add_argument("--remote-preset", default="ollama",
                    help="remote-cli preset: openrouter, ollama, or local")
    ap.add_argument("--remote-model", default=None,
                    help="remote-cli model id (overrides the preset default)")
    args = ap.parse_args()

    out_dir = Path(args.out)
    out_dir.mkdir(exist_ok=False, parents=True)
    names = args.models.split(",")
    unknown = [n for n in names if n not in M.ADAPTERS]
    if unknown:
        ap.error(f"unknown models: {unknown}")

    specs = []
    for s in args.sets.split(","):
        specs.extend(load_specs(SETS[s]))

    manifest = {"started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "script_sha256": sha256(__file__),
                "models_py_sha256": sha256(HERE / "models.py"),
                "set_sha256": {k: sha256(v) for k, v in SETS.items() if k in args.sets},
                "platform": platform.platform(), "python": sys.version.split()[0],
                "models": {}}
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")

    for name in names:
        cls = M.ADAPTERS[name]
        if name == "remote-cli":
            adapter = cls(preset=args.remote_preset, model=args.remote_model,
                          binary=args.pixel_bin)
        else:
            adapter = cls()
        print(f"== {name} ==", flush=True)
        try:
            info = run_model(name, adapter, specs, out_dir)
        except Exception as e:  # noqa: BLE001
            info = {"fatal": f"{type(e).__name__}: {e}"}
            print(f"   FATAL {info['fatal']}", flush=True)
        for attr in ("preset", "model", "resolved_model"):
            if hasattr(adapter, attr):
                info[attr] = getattr(adapter, attr)
        manifest["models"][name] = info
        (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")
        print(f"   {info}", flush=True)

    manifest["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")


if __name__ == "__main__":
    main()
