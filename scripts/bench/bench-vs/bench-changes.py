#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Working-tree change mapping: `pixel what-changed` vs `gitnexus detect-changes`.

Ground truth is constructed, not inferred: the harness edits a known set of
functions (one inert comment line inside each body), so the set of symbols that
genuinely changed is exactly the set it wrote. Both tools are then asked to map
the dirty tree back to symbols and scored on that set. The edits are reverted
in a finally block whether or not the run succeeds.
"""
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_common import gitnexus_cli, positional  # noqa: E402

REPS = 3
MARKER = "// pixel-vs-gitnexus benchmark edit"


def run(cmd, cwd):
    with tempfile.NamedTemporaryFile(delete=False) as fh:
        tmp = fh.name
    try:
        with open(tmp, "wb") as out:
            t0 = time.perf_counter()
            p = subprocess.run(cmd, cwd=cwd, stdout=out,
                               stderr=subprocess.DEVNULL, timeout=600)
            ms = (time.perf_counter() - t0) * 1000
        return ms, Path(tmp).read_bytes(), p.returncode
    finally:
        Path(tmp).unlink(missing_ok=True)


def snapshot(repo, targets):
    """Read each target's exact bytes, refusing to touch an already-dirty file.

    The restore path used to be `git checkout -- <files>`, which throws away
    whatever the developer had uncommitted in those files. A benchmark must not
    destroy work to measure something, so the originals are kept in memory and
    written back verbatim. Running against a file that is already modified is
    refused outright rather than snapshotted: the edit would land on top of
    uncommitted work, and both tools would then be scored on a diff that is not
    the one this harness constructed.
    """
    files = sorted({t["file"] for t in targets})
    dirty = subprocess.run(["git", "status", "--porcelain", "--"] + files,
                           cwd=repo, capture_output=True, text=True).stdout.strip()
    if dirty:
        sys.exit(f"refusing to run: target files already modified\n{dirty}\n"
                 "commit or stash them first — this harness rewrites them.")
    return {f: (repo / f).read_bytes() for f in files}


def restore(repo, original):
    for rel, data in original.items():
        (repo / rel).write_bytes(data)


def edit(repo, targets):
    """Insert one comment line immediately after each target's opening brace."""
    for t in targets:
        path = repo / t["file"]
        lines = path.read_text().splitlines(keepends=True)
        idx = t["line"] - 1
        while idx < len(lines) and "{" not in lines[idx]:
            idx += 1
        lines.insert(idx + 1, f"    {MARKER}\n")
        path.write_text("".join(lines))


def parse_gitnexus_changes(raw):
    """`gitnexus detect-changes` has no --json flag: its CLI prints a report.

    The "Changed symbols:" block lists one `<Kind> <name> -> <file>` entry per
    symbol, which is the same information pixel returns as JSON. Parsing it is
    the only way to score the two tools on the same question; scoring GitNexus
    as if it had answered nothing would measure the output format, not the tool.
    """
    text = raw.decode(errors="replace")
    names, in_block = set(), False
    for line in text.splitlines():
        if line.startswith("Changed symbols:"):
            in_block = True
            continue
        if in_block:
            if not line.startswith("  "):
                break
            parts = line.strip().split()
            if len(parts) >= 2:
                names.add(parts[1])
    return names


def main():
    args = positional()
    GN_CLI = gitnexus_cli()
    repo = Path(args[0]).resolve()
    targets = json.load(open(args[1]))
    truth = {t["symbol"] for t in targets}
    original = snapshot(repo, targets)
    try:
        edit(repo, targets)
        dirty = subprocess.run(["git", "status", "--porcelain"], cwd=repo,
                               capture_output=True, text=True).stdout.strip()
        assert dirty, "edits did not dirty the tree - harness bug, not a result"
        rows = {"truth": sorted(truth), "dirty_files": len(dirty.splitlines())}
        for tool, cmd, extract in (
            ("pixel", ["pixel", "what-changed", "--metrics", "off"],
             lambda d: {e.get("name") for e in d.get("symbols", [])
                        if isinstance(e, dict)}),
            ("gitnexus", GN_CLI + ["detect-changes", "--scope", "unstaged"],
             parse_gitnexus_changes),
        ):
            times, out, rc = [], b"", 0
            for i in range(REPS + 1):
                ms, out, rc = run(cmd, repo)
                if i:
                    times.append(ms)
            try:
                found = extract(out) if tool == "gitnexus" else extract(json.loads(out))
            except Exception as e:  # noqa: BLE001
                rows[f"{tool}_parse_error"] = f"{type(e).__name__}: {e}"
                found = set()
            times.sort()
            rows.update({
                f"{tool}_ms_p50": round(times[len(times) // 2], 1),
                f"{tool}_bytes": len(out),
                f"{tool}_rc": rc,
                f"{tool}_found": sorted(found & truth),
                f"{tool}_recall": round(len(found & truth) / len(truth), 3),
                f"{tool}_raw_head": out[:400].decode(errors="replace"),
            })
            print(f"  {tool:9s} recall={rows[f'{tool}_recall']:.2f} "
                  f"{rows[f'{tool}_ms_p50']}ms {rows[f'{tool}_bytes']}B",
                  file=sys.stderr)
        json.dump(rows, sys.stdout, indent=2)
    finally:
        restore(repo, original)
        left = subprocess.run(
            ["git", "status", "--porcelain", "--"] + sorted(original),
            cwd=repo, capture_output=True, text=True).stdout.strip()
        print(f"\n[restore] target files now: {left or 'as before the run'}",
              file=sys.stderr)


if __name__ == "__main__":
    main()
