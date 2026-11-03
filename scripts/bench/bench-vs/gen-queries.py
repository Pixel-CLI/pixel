#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Build a natural-language retrieval set from the repo's OWN doc comments.

CodeSearchNet's design, applied locally: the query is a doc comment written by
the repository's maintainers, and the relevant document is the file that comment
documents. Neither the benchmark author nor any tool under test wrote the
queries, which is the whole point -- hand-written queries would encode whichever
retrieval model the author had in mind.

To stop the task collapsing into a lexical name lookup, every word of the
documented symbol's identifier and of the file's own basename is stripped from
the query. What remains is prose about behaviour. A case is kept only when at
least MIN_WORDS words survive, and at most one case is taken per file so the set
does not cluster on whichever file happens to be the best documented.
"""
import ast
import json
import os
import re
import subprocess
import sys
from pathlib import Path

MIN_WORDS = 8
MAX_WORDS = 60

LANGS = {
    "rust": {"exts": (".rs",), "comment": r"^\s*///\s?(.*)$",
             "decl": r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|struct|enum|trait)\s+(\w+)"},
    "typescript": {"exts": (".ts", ".tsx"), "comment": r"^\s*\*\s?(.*)$",
                   "decl": r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?"
                           r"(?:function|class|interface|const|type)\s+(\w+)"},
    "ruby": {"exts": (".rb",), "comment": r"^\s*#\s?(.*)$",
             "decl": r"^\s*(?:def\s+(?:self\.)?|class\s+|module\s+)(\w+)"},
    # Python is handled by `python_cases` below, not by the line scanner: a
    # regex for a docstring matches every line of the file, which made the
    # scanner `continue` past every declaration and emit no cases at all.
    "python": {"exts": (".py",), "comment": None, "decl": None},
}

WORD = re.compile(r"[A-Za-z]{3,}")


def ident_words(name):
    parts = re.split(r"[_\-]", name)
    out = []
    for p in parts:
        out += re.findall(r"[A-Z]+(?![a-z])|[A-Z][a-z]*|[a-z]+", p)
    return {w.lower() for w in out if len(w) >= 3}


def python_cases(rel, source):
    """Docstrings via the AST, which is the only way to know one IS a docstring."""
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return None
    for node in ast.walk(tree):
        if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            continue
        doc = ast.get_docstring(node)
        if not doc:
            continue
        banned = ident_words(node.name) | ident_words(Path(rel).stem)
        words = [w for w in WORD.findall(doc) if w.lower() not in banned]
        if MIN_WORDS <= len(words) <= MAX_WORDS:
            return {"query": " ".join(words[:MAX_WORDS]), "truth_file": rel,
                    "symbol": node.name, "lang": "python"}
    return None


def spread(cases, limit):
    """Evenly spaced indices across the WHOLE candidate list.

    The previous `cases[::len//limit]` collapsed to a stride of 1 whenever the
    candidate count was under twice the limit, which silently took the first
    `limit` path-sorted cases and biased the corpus toward early directories.
    """
    if len(cases) <= limit:
        return cases
    if limit == 1:
        return cases[:1]
    idx = sorted({round(i * (len(cases) - 1) / (limit - 1)) for i in range(limit)})
    return [cases[i] for i in idx]


def main():
    repo = Path(sys.argv[1]).resolve()
    lang = sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 15
    exclude = set(sys.argv[4].split(",")) if len(sys.argv) > 4 else set()

    spec = LANGS[lang]
    com_re = re.compile(spec["comment"]) if spec["comment"] else None
    decl_re = re.compile(spec["decl"]) if spec["decl"] else None
    rels = [r for r in subprocess.run(["git", "ls-files"], cwd=repo,
                                      capture_output=True, text=True).stdout.splitlines()
            if r.endswith(spec["exts"])
            and not any(p in exclude for p in Path(r).parts)]

    cases = []
    for rel in rels:
        try:
            lines = (repo / rel).read_text(errors="replace").splitlines()
        except OSError:
            continue
        if lang == "python":
            c = python_cases(rel, "\n".join(lines))
            if c:
                cases.append(c)
            continue
        buf = []
        for line in lines:
            m = com_re.match(line)
            if m:
                buf.append(m.group(1).strip())
                continue
            d = decl_re.match(line)
            if d and buf:
                banned = ident_words(d.group(1)) | ident_words(Path(rel).stem)
                words = [w for w in WORD.findall(" ".join(buf))
                         if w.lower() not in banned]
                if MIN_WORDS <= len(words) <= MAX_WORDS:
                    cases.append({"query": " ".join(words[:MAX_WORDS]),
                                  "truth_file": rel, "symbol": d.group(1),
                                  "lang": lang})
                    break          # one case per file
            buf = []
    # Deterministic, corpus-independent sample: sort by query text, take a
    # strided slice so cases are spread across the tree rather than clustered
    # in whichever directory sorts first.
    cases.sort(key=lambda c: c["truth_file"])
    picked = spread(cases, limit)
    json.dump(picked, sys.stdout, indent=2)
    print(f"{len(cases)} candidates -> {len(picked)} cases", file=sys.stderr)


if __name__ == "__main__":
    main()
