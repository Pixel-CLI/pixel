#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Generate impact/caller ground truth for the pixel-vs-GitNexus benchmark.

Ground truth is deliberately mechanical and re-derivable: for a symbol with
exactly ONE definition in the repo, the truth set is the set of tracked files
that contain a *call site* of that symbol on a line that is not its own
definition. File granularity, not line granularity, because that is the one
level both tools report in a comparable shape.

A case is kept only when the symbol is unambiguous (one definition) and its
truth set is small enough to be checked by hand (<= MAX_FILES) and large
enough to be interesting (>= MIN_FILES). Both tools are scored against the
same set with the same scorer.
"""
import os
import re
import subprocess
import sys
import json
from pathlib import Path
from collections import defaultdict

# Defaults suit statically-typed corpora. Ruby resolves far fewer names
# unambiguously, so its committed fixtures were generated with a wider window --
# `MIN_FILES=2 MAX_FILES=12` -- which is why they hold truth sets of 2 and 10.
# Regenerating them with the defaults produces a different, smaller set; the
# exact commands are recorded in docs/bench/vs-gitnexus/cases/REGENERATE.md.
MIN_FILES = int(os.environ.get("MIN_FILES", 3))
MAX_FILES = int(os.environ.get("MAX_FILES", 8))

LANGS = {
    "rust": {
        "exts": (".rs",),
        "def": r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w{8,40})\s*[(<]",
    },
    "ruby": {
        # Ruby calls frequently omit parentheses, so the call-site index below
        # under-counts real callers. The resulting truth set stays a SUBSET of
        # the true one: recall is scorable, precision is not (a tool that finds
        # a paren-less caller would be punished for being right).
        "exts": (".rb",),
        "def": r"^\s*def\s+(?:self\.)?(\w{8,40})\s*[(\n]?",
    },
    "typescript": {
        "exts": (".ts", ".tsx"),
        "def": r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+(\w{8,40})\s*[(<]"
               r"|^\s*(?:export\s+)?(?:const|let)\s+(\w{8,40})\s*(?::[^=]+)?=\s*(?:async\s*)?\(",
    },
}


def strip_test_blocks(body, lang):
    """Blank out `#[cfg(test)] mod ... { }` bodies in Rust sources.

    Both tools exclude test code from impact analysis by default, so a call
    site that only exists inside an in-file test module is not something either
    is expected to report. Leaving it in the truth set does not favour one tool
    over the other, but it does depress both recalls against a target neither
    is aiming at. Lines are blanked rather than removed so line numbers, and
    therefore the recorded definition sites, stay accurate.
    """
    if lang != "rust":
        return body
    lines = body.splitlines(keepends=True)
    out, i = [], 0
    while i < len(lines):
        if lines[i].strip().startswith("#[cfg(test)]"):
            depth, seen = 0, False
            while i < len(lines):
                depth += lines[i].count("{") - lines[i].count("}")
                seen = seen or "{" in lines[i]
                out.append("\n")
                i += 1
                if seen and depth <= 0:
                    break
            continue
        out.append(lines[i])
        i += 1
    return "".join(out)


def tracked_files(repo, exts, exclude):
    out = subprocess.run(["git", "ls-files"], cwd=repo, capture_output=True, text=True).stdout
    files = []
    for rel in out.splitlines():
        if not rel.endswith(exts):
            continue
        if any(part in exclude for part in Path(rel).parts):
            continue
        files.append(rel)
    return files


def main():
    repo = Path(sys.argv[1]).resolve()
    lang = sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 8
    exclude = set(sys.argv[4].split(",")) if len(sys.argv) > 4 else set()

    spec = LANGS[lang]
    def_re = re.compile(spec["def"])
    files = tracked_files(repo, spec["exts"], exclude)

    text = {}
    for rel in files:
        try:
            text[rel] = strip_test_blocks((repo / rel).read_text(errors="replace"), lang)
        except OSError:
            continue

    # definitions: symbol -> [(file, line)]
    defs = defaultdict(list)
    for rel, body in text.items():
        for i, line in enumerate(body.splitlines(), 1):
            m = def_re.match(line)
            if m:
                name = next(g for g in m.groups() if g)
                defs[name].append((rel, i))

    # Inverted index: identifier-called-as-a-function -> files containing it.
    # Built once over the corpus, so truth lookup is O(1) per symbol instead of
    # a fresh scan of every file (which is minutes on a 5k-file repo).
    call_tok = re.compile(r"(?<![\w.])(\w{8,40})\s*\(")
    calls = defaultdict(set)      # name -> {file}
    def_lines = defaultdict(set)  # file -> {lineno of any definition}
    for rel, body in text.items():
        for i, line in enumerate(body.splitlines(), 1):
            if def_re.match(line):
                def_lines[rel].add(i)
                continue          # a definition line is never a call site
            for name in call_tok.findall(line):
                calls[name].add(rel)

    cases = []
    for name, places in sorted(defs.items()):
        if len(places) != 1:
            continue  # ambiguous: measuring disambiguation, not caller recall
        def_file, def_line = places[0]
        truth = calls.get(name, set())
        if MIN_FILES <= len(truth) <= MAX_FILES:
            cases.append({
                "symbol": name,
                "lang": lang,
                "def_file": def_file,
                "def_line": def_line,
                "truth_files": sorted(truth),
            })

    # Prefer the most distinctive names (longest) so neither tool is scored on
    # a name that collides with unrelated identifiers in the other language.
    cases.sort(key=lambda c: (-len(c["symbol"]), c["symbol"]))
    print(json.dumps(cases[:limit], indent=2))


if __name__ == "__main__":
    main()
