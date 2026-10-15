#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""The probe chain: every probe of ``probes/`` that applies, run in parallel inside its budget, merged into the brief.

Until the brief itself runs probes (the Rust integration), this is the reference implementation of that step and
what ``arena.sh run-set --brief-extra`` hangs on the frozen binary's prompt-submit hook. It speaks the wrapper
protocol of ``probe-test.py``'s hook shim: the environment carries ``CLONE`` (the repository), ``PROMPT`` and
``BRIEF_FILE`` (the brief the binary printed); the output is nothing (no change) or ``@@REPLACE@@`` followed by the
merged brief. The kind and the gate are read from the last line of ``.pixel/brief-decisions.jsonl`` that the hook
just wrote; a closed gate runs no probe. A probe that times out or fails is skipped.

    probe-chain.py                      (environment as above)
    probe-chain.py --only a,b           only these probes
    probe-chain.py --self-test
"""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKTREE = HERE.parent


def load_probe_test():
    spec = importlib.util.spec_from_file_location("probe_test", HERE / "probe-test.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules["probe_test"] = module
    spec.loader.exec_module(module)
    return module


def last_decision(root):
    """``(kind, gate)`` of the last decision the hook logged in ``root``."""
    try:
        lines = [l for l in (Path(root) / ".pixel" / "brief-decisions.jsonl").read_text().splitlines() if l.strip()]
        rec = json.loads(lines[-1])
        return rec.get("kind"), rec.get("gate")
    except (OSError, ValueError, IndexError):
        return None, None


def chain(pt, probes_dir, root, prompt, brief, kind, pixel_bin, only=None):
    """The merged brief, or ``None`` when no probe added anything."""
    home = tempfile.mkdtemp(prefix="chain-home-")
    probes = [pt.Probe(d) for d in sorted(Path(probes_dir).glob("*/")) if not d.name.startswith(("_", "."))]
    probes = [p for p in probes if not p.problems and (not only or p.name in only) and p.applies(kind, prompt)]
    request = {"prompt": prompt, "repo_root": str(root), "brief": brief or "[PIXEL:BRIEF]\n", "kind": kind,
               "session_files": []}
    outputs = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(probes))) as pool:
        futures = {pool.submit(pt.run_probe, p, request, pixel_bin, home): p for p in probes}
        for future, probe in futures.items():
            stdout, _ms, error = future.result()
            if not error and stdout.strip():
                outputs.append((probe.name, probe.mode, stdout))
    return pt.merge(brief or "[PIXEL:BRIEF]\n", outputs) if outputs else None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--only", default=None)
    parser.add_argument("--probes", default=str(WORKTREE / "probes"))
    parser.add_argument("--bin", default=os.environ.get("PIXEL_REAL") or "pixel")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return 0 if unittest.TextTestRunner(verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)).wasSuccessful() else 1
    root, prompt = os.environ["CLONE"], os.environ["PROMPT"]
    brief = Path(os.environ["BRIEF_FILE"]).read_text()
    kind, gate = last_decision(root)
    if gate not in (None, "open"):
        return 0
    merged = chain(load_probe_test(), args.probes, root, prompt, brief, kind, args.bin,
                   set(args.only.split(",")) if args.only else None)
    if merged:
        sys.stdout.write("@@REPLACE@@\n" + merged)
    return 0


class SelfTest(unittest.TestCase):
    def probe(self, base, name, body, extra=""):
        directory = Path(base) / name
        directory.mkdir()
        (directory / "pixel-plugin.toml").write_text(
            f'name = "{name}"\nversion = "0.1.0"\napi = 1\ndescription = "d"\nrun = "probe"\n'
            f'capabilities = ["brief-probe"]\n\n[brief-probe]\nowner = "o"\nbudget_ms = 500\nmode = "append"\n{extra}')
        script = directory / "probe"
        script.write_text(body)
        script.chmod(0o755)

    def test_the_chain_runs_the_probes_that_apply_and_merges_their_blocks_in_name_order(self):
        pt = load_probe_test()
        with tempfile.TemporaryDirectory() as tmp:
            self.probe(tmp, "b-add", "#!/bin/sh\ncat >/dev/null\nprintf '### b.rs:1-1 — b\\n1| b\\n'\n")
            self.probe(tmp, "a-add", "#!/bin/sh\ncat >/dev/null\nprintf '### a.rs:1-1 — a\\n1| a\\n'\n")
            self.probe(tmp, "c-skip", "#!/bin/sh\ncat >/dev/null\nprintf '### c.rs:1-1 — c\\n'\n",
                       extra='when_kinds = ["history"]\n')
            self.probe(tmp, "d-fail", "#!/bin/sh\nexit 2\n")
            merged = chain(pt, tmp, tmp, "a question", "[PIXEL:BRIEF]\n### z.rs:1-1 — z\n1| z\n", "flow", "pixel")
            headers = [l[4:] for l in merged.splitlines() if l.startswith("### ")]
            self.assertEqual(headers, ["z.rs:1-1 — z", "a.rs:1-1 — a", "b.rs:1-1 — b"])

    def test_the_chain_says_nothing_when_no_probe_adds_anything(self):
        pt = load_probe_test()
        with tempfile.TemporaryDirectory() as tmp:
            self.probe(tmp, "quiet", "#!/bin/sh\ncat >/dev/null\n")
            self.assertIsNone(chain(pt, tmp, tmp, "q", "[PIXEL:BRIEF]\n", None, "pixel"))

    def test_the_last_decision_gives_the_kind_and_gate_and_a_missing_log_gives_none(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(last_decision(tmp), (None, None))
            (Path(tmp) / ".pixel").mkdir()
            (Path(tmp) / ".pixel" / "brief-decisions.jsonl").write_text(
                json.dumps({"kind": "old", "gate": "closed"}) + "\n" + json.dumps({"kind": "flow", "gate": "open"}) + "\n")
            self.assertEqual(last_decision(tmp), ("flow", "open"))


if __name__ == "__main__":
    sys.exit(main())
