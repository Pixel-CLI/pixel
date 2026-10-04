#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Repo-map comparison: `stacklit derive` vs `pixel repo-map --markdown`.

These two optimise opposite ends of the same trade-off, so the benchmark
reports BOTH axes and refuses to collapse them into one score:

  size     -- bytes/tokens the map costs to carry
  coverage -- share of the repo's tracked source files the map names by path

A map that is small because it names almost nothing is not "efficient", and a
map that covers everything is not "better" if no agent can afford to load it.
A command that exits non-zero scores no coverage at all: crediting a failed run
with whatever it managed to print would be the same mistake in miniature.
Coverage is measured by substring match of each tracked source path (and of its
basename, since a module-level map may name a directory rather than a file).
"""
import json
import re
import subprocess
import sys
import time
from pathlib import Path

SRC_EXT = (".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".rb", ".go", ".java")


def run(cmd, cwd):
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, timeout=900)
    return (time.perf_counter() - t0) * 1000, p.stdout, p.returncode


def _names_dir(text, rel):
    """Does the map actually name this file's directory?

    A plain substring test is wrong twice over. `Path(f).parent` is "." for a
    root-level file, and any map containing a full stop then counts it. And for
    a nested file, "lib/core" matches inside "lib/core_extra", crediting the map
    for a directory it never names. So the match is anchored on complete path
    components, and a root-level file counts only when the map names the root
    explicitly.
    """
    parent = str(Path(rel).parent)
    if parent == ".":
        return bool(re.search(r"(?:^|[\s\"'`(\[])(?:\./|<root>|repository root)",
                              text, re.IGNORECASE))
    return re.search(r"(?:^|[^\w/])" + re.escape(parent) + r"(?:[/\s\"'`)\],]|$)",
                     text) is not None


def coverage(text, repo):
    files = [f for f in subprocess.run(["git", "ls-files"], cwd=repo,
                                       capture_output=True, text=True)
             .stdout.splitlines() if f.endswith(SRC_EXT)]
    by_path = sum(1 for f in files
                  if re.search(r"(?:^|[^\w/])" + re.escape(f) + r"(?:[\s\"'`)\],]|$)",
                               text))
    by_dir = sum(1 for f in files if _names_dir(text, f))
    return {"source_files": len(files),
            "named_by_path": by_path,
            "path_coverage": round(by_path / len(files), 4) if files else None,
            "dir_named": by_dir,
            "dir_coverage": round(by_dir / len(files), 4) if files else None}


def main():
    repo = Path(sys.argv[1]).resolve()
    stacklit = sys.argv[2]
    out = {"repo": repo.name}

    # `generate` builds the index `derive` reads, so its exit code is part of the
    # result: swallowing it would let a failed index be reported as a cheap map.
    gen_ms, _, gen_rc = run([stacklit, "generate"], repo)
    ms, raw, rc = run([stacklit, "derive"], repo)
    text = raw.decode(errors="replace") if (rc == 0 and gen_rc == 0) else ""
    out["stacklit"] = {"generate_ms": round(gen_ms), "derive_ms": round(ms),
                       "generate_rc": gen_rc, "rc": rc, "bytes": len(raw),
                       "approx_tokens": len(raw) // 4, **coverage(text, repo)}

    ms, raw, rc = run(["pixel", "repo-map", "--markdown", "--metrics", "off"], repo)
    text = raw.decode(errors="replace") if rc == 0 else ""
    out["pixel_repo_map"] = {"ms": round(ms), "rc": rc, "bytes": len(raw),
                             "approx_tokens": len(raw) // 4,
                             **coverage(text, repo)}

    ms, raw, rc = run(["pixel", "list-areas", "--metrics", "off"], repo)
    text = raw.decode(errors="replace") if rc == 0 else ""
    out["pixel_list_areas"] = {"ms": round(ms), "rc": rc, "bytes": len(raw),
                               "approx_tokens": len(raw) // 4,
                               **coverage(text, repo)}

    json.dump(out, sys.stdout, indent=2)
    for k in ("stacklit", "pixel_repo_map", "pixel_list_areas"):
        d = out[k]
        print(f"  {k:18s} {d['approx_tokens']:>8d} tok  "
              f"path-cov {str(d['path_coverage']):>7s}  "
              f"dir-cov {str(d['dir_coverage']):>7s}  rc={d['rc']}",
              file=sys.stderr)


if __name__ == "__main__":
    main()
