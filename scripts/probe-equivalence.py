#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Is the brief that runs probes itself the brief the Python reference chain makes? (issue #906, local only)

For each census question, in its pinned clone: the native binary's brief with probes OFF (PIXEL_BRIEF_PROBES=0) is the
input; ``scripts/probe-chain.py``'s chain (the reference merge of ``probe-test.py``) runs the same probes on it; the
native binary's brief with probes ON must equal that, byte for byte. The brief is what the three prompt-submit hook parts deliver. The probes are found under ``--home``
(``pixel setup`` wrote them there; the real HOME is never touched). Prompts are read from census.db and never printed.

    probe-equivalence.py --bin <native pixel> --home <fake home> [--ids a,b]
"""

import argparse
import importlib.util
import json
import os
import subprocess
import sys
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def native(binary, clone, prompt, home, probes):
    """The brief a Codex host receives: the three prompt-submit hook parts run together, their contexts joined, plus
    the last decision the hook logged (its kind, gate and probe runs)."""
    env = {**os.environ, "HOME": str(home), "PIXEL_BRIEF_PROBES": "1" if probes else "0"}
    env.pop("ANTHROPIC_API_KEY", None)
    payload = json.dumps({"hook_event_name": "UserPromptSubmit", "session_id": "equiv-" + uuid.uuid4().hex, "cwd": str(clone), "prompt": prompt})
    procs = [subprocess.Popen([binary, "run-hook", "task-event", "--provider", "codex", "--event", "prompt-submit",
                               "--part", f"{k}/3"], cwd=clone, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              text=True) for k in (1, 2, 3)]
    outs = []
    for proc in procs:
        try:
            outs.append(proc.communicate(payload, timeout=60)[0])
        except subprocess.TimeoutExpired:
            proc.kill()
            outs.append("")   # a hook that did not answer is a part the host would not have received
    text = ""
    for out in outs:
        try:
            text += (json.loads(out).get("hookSpecificOutput") or {}).get("additionalContext", "")
        except ValueError:
            pass
    meta = {}
    try:
        lines = [l for l in (Path(clone) / ".pixel" / "brief-decisions.jsonl").read_text().splitlines() if l.strip()]
        meta = json.loads(lines[-1])
    except (OSError, ValueError, IndexError):
        pass
    return (text or None), meta


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--bin", required=True)
    parser.add_argument("--home", required=True)
    parser.add_argument("--ids", default=None)
    parser.add_argument("--root", default="/private/tmp/claude-501/census")
    parser.add_argument("--census-dir", default=None)
    args = parser.parse_args(argv)
    bc, chain = load("brief_check", "brief-check.py"), load("probe_chain", "probe-chain.py")
    pt = chain.load_probe_test()
    sb = bc.load_module("score_brief", args.census_dir)
    binary = str(Path(args.bin).resolve())
    ids = args.ids.split(",") if args.ids else bc.question_ids(args.root)
    probes_dir = Path(args.home) / ".pixel" / "plugins"
    equal = 0
    for qid in ids:
        _name, prompt, clone = bc.prepare(Path(args.root), sb, qid, binary)
        for _ in range(3):   # a daemon that has just started answers empty first
            base, meta0 = native(binary, clone, prompt, args.home, False)
            if base:
                break
        on, meta = native(binary, clone, prompt, args.home, True)
        gate_open = (meta0.get("gate") or "open") == "open"
        expected = chain.chain(pt, probes_dir, clone, prompt, base, meta0.get("kind"), binary) if gate_open else None
        expected = expected if expected is not None else base
        ran = [r["name"] + ":" + r["outcome"] for r in meta.get("probes", [])]
        same = (on or "") == (expected or "")
        equal += same
        print(f"{qid:<13} {'EQUAL' if same else 'DIFFERENT':<9} bytes {len((on or '').encode())} (base {len((base or '').encode())}) "
              f"probes {','.join(ran) or '-'}")
        if not same:
            a, b = (on or "").splitlines(), (expected or "").splitlines()
            for i, (x, y) in enumerate(zip(a, b)):
                if x != y:
                    print(f"  first difference at line {i + 1}")
                    break
            else:
                print(f"  lengths differ: native {len(a)} lines, reference {len(b)} lines")
    print(f"{equal} of {len(ids)} identical")
    return 0 if equal == len(ids) else 1


if __name__ == "__main__":
    sys.exit(main())
