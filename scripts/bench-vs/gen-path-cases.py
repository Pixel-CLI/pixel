#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Generate call-path cases: (caller, callee) pairs with a located call site.

A pair is kept only when both names have exactly one **function** definition in
the repo. That is a narrower namespace than either tool's symbol table, which
also holds modules and commands: `branch`, `update` and `doctor` pass this
filter and are still ambiguous to pixel and GitNexus alike. Those cases are kept
on purpose -- `bench-path.py` scores "ambiguity reported" as its own verdict,
separate from found/not-found, because refusing to guess is correct behaviour
and worth measuring. What this filter buys is that no pair is ambiguous merely
because two *functions* share a name.

In-file `#[cfg(test)]` blocks are stripped first, so a test helper never becomes
a case.
"""
import importlib.util
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

# gen-truth.py is not an importable module name, so load it by path and reuse
# its test-block stripper rather than keeping a second copy of that rule.
_spec = importlib.util.spec_from_file_location(
    "gentruth", Path(__file__).parent / "gen-truth.py")
gentruth = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gentruth)

DEF_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)\s*[(<]")


def main():
    repo = Path(sys.argv[1]).resolve()
    callee_cases = json.load(open(sys.argv[2]))
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 6

    rels = subprocess.run(["git", "ls-files", "*.rs"], cwd=repo,
                          capture_output=True, text=True).stdout.split()
    bodies, defs = {}, defaultdict(int)
    for rel in rels:
        body = gentruth.strip_test_blocks(
            (repo / rel).read_text(errors="replace"), "rust")
        bodies[rel] = body.splitlines()
        for line in bodies[rel]:
            m = DEF_RE.match(line)
            if m:
                defs[m.group(1)] += 1

    seen, out = set(), []
    for c in callee_cases:
        callee = c["symbol"]
        if defs[callee] != 1:
            continue
        call_re = re.compile(r"(?<![\w.])" + re.escape(callee) + r"\s*\(")
        for rel in c["truth_files"]:
            cur = None
            for i, line in enumerate(bodies.get(rel, []), 1):
                m = DEF_RE.match(line)
                if m:
                    cur = m.group(1)
                    continue
                if call_re.search(line) and cur and defs[cur] == 1 and cur != callee:
                    key = (cur, callee)
                    if key not in seen:
                        seen.add(key)
                        out.append({"from": cur, "to": callee, "from_file": rel,
                                    "to_file": c["def_file"], "verified_line": i})
                    break
    json.dump(out[:limit], sys.stdout, indent=2)


if __name__ == "__main__":
    main()
