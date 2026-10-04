#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""d2-5 — aggregate what-changed outputs into abstention rates per motif.

A commit "abstains" when a whole-change negative from `diff-reaches` would be
downgraded to `unknown`: `uncovered_changes` or `unanchored` is non-empty
(docs/design/evaluate.md, C3). `outside_symbol` residues are split by reading
the lines themselves at the commit (new side) or its parent (old side).
"""
import collections
import json
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).parent

TEST_PATH = re.compile(
    r"(^|/)(tests?|spec|specs|__tests__|test-utils|fixtures?)/|_test\.[a-z]+$|\.test\.[a-z]+$|\.spec\.[a-z]+$|_spec\.rb$"
)
COMMENT = re.compile(r"^\s*(//|/\*|\*|\*/|--|#(?!\[)|<!--)")
IMPORT = re.compile(
    r"^\s*(pub(\([a-z]+\))?\s+)?(use\s|import\s|from\s+\S+\s+import|require(_relative)?[\s(]|export\s+(\*|\{[^}]*\}|type\s+\{[^}]*\})\s+from|mod\s+\w+;|extern\s+crate|\}\s*from\s|[\w{},\s]*\}?\s*from\s+['\"])"
)
ATTRIBUTE = re.compile(r"^\s*(#!?\[|@[A-Za-z])")


def show(tree, rev, path, cache={}):
    key = (tree, rev, path)
    if key not in cache:
        r = subprocess.run(["git", "-C", str(tree), "show", f"{rev}:{path}"], capture_output=True)
        cache[key] = r.stdout.decode("utf-8", "replace").splitlines() if r.returncode == 0 else None
    return cache[key]


def file_kind(path):
    name = path.rsplit("/", 1)[-1].lower()
    ext = name.rsplit(".", 1)[-1] if "." in name else ""
    if ext in {"md", "mdx", "txt", "rst", "adoc"} or name in {"license", "changelog", "readme"}:
        return "docs"
    if ext in {"toml", "yml", "yaml", "json", "jsonc", "lock", "ini", "cfg", "conf", "env", "plist", "xml", "gemspec", "gitignore", "gitattributes", "npmrc", "nvmrc", "editorconfig"} or name.startswith((".env", "dockerfile", "gemfile", "procfile", "makefile", "justfile", ".")):
        return "config"
    if ext in {"erb", "haml", "slim", "hbs", "liquid", "jinja", "njk", "mustache"}:
        return "templates"
    if ext in {"css", "scss", "sass", "less", "html", "svg", "png", "jpg", "jpeg", "gif", "webp", "ico", "woff", "woff2", "ttf"}:
        return "style_or_asset"
    if ext in {"sql"}:
        return "sql"
    if ext in {"sh", "bash", "fish", "zsh", "ps1"}:
        return "shell"
    return "other:" + (ext or name)


def touches(tree, commit, path):
    r = subprocess.run(["git", "-C", str(tree), "diff", "--quiet", f"{commit}^", commit, "--", path])
    assert r.returncode in (0, 1), f"git diff failed in {tree} for {commit}"
    return r.returncode == 1


def rust_test_start(lines):
    for i, line in enumerate(lines):
        if line.strip().startswith("#[cfg(test)]"):
            return i + 1  # 1-based line of the attribute
    return None


def classify(tree, commit, change):
    path = change["path"]
    if TEST_PATH.search(path):
        return "test_code"
    if change.get("new_lines"):
        rev, (a, b) = commit, change["new_lines"]
    else:
        rev, (a, b) = f"{commit}^", change["old_lines"]
    lines = show(tree, rev, path)
    assert lines is not None, f"cannot read {rev}:{path} in {tree}"
    if path.endswith(".rs"):
        start = rust_test_start(lines)
        if start is not None and a >= start:
            return "test_code"
    body = [l for l in lines[a - 1 : b] if l.strip()]
    if not body:
        return "comment_or_blank"
    if all(COMMENT.match(l) for l in body):
        return "comment_or_blank"
    if all(COMMENT.match(l) or IMPORT.match(l) for l in body):
        return "import"
    if all(COMMENT.match(l) or ATTRIBUTE.match(l) for l in body):
        return "attribute"
    return "other_code"


def main(pairs):
    report = {}
    coverage = {}
    for pair in pairs:
        name, _, repo = pair.partition("=")
        out = ROOT / "out" / name
        # Any clone holding the measured commits: the lines are read from git
        # objects, so the working tree it was measured in need not survive.
        tree = pathlib.Path(repo).expanduser()
        assert tree.is_dir(), f"object store for {name} missing: {tree}"
        listed = (out / "commits.txt").read_text().split()
        rows = []
        for f in sorted(out.glob("[0-9][0-9][0-9]-*.json")):
            commit = f.stem.split("-", 1)[1]
            try:
                d = json.loads(f.read_text())
            except json.JSONDecodeError:
                rows.append({"commit": commit, "error": True})
                continue
            motifs = collections.Counter()
            for u in d.get("uncovered_changes", []):
                if u["path"].endswith(".gitignore") and not touches(tree, commit, u["path"]):
                    continue  # pixel's own `.pixel/` append, not the commit's change
                m = u["motif"]
                if m == "unsupported_language":
                    m = "unsupported_language/" + file_kind(u["path"])
                if m == "outside_symbol":
                    m = "outside_symbol/" + classify(tree, commit, u)
                motifs[m] += 1
            if d.get("unanchored"):
                motifs["unanchored"] += len(d["unanchored"])
            rows.append({
                "commit": commit,
                "symbols": d.get("symbols_total", len(d.get("symbols", []))),
                "files": d.get("changed_files", 0),
                "motifs": dict(motifs),
                "lower_bound": d.get("uncovered_lower_bound", False),
                "build": d.get("graph_build"),
            })
        # A commit with no JSON was either never reached (a run cut short:
        # the missing ones form the tail of commits.txt) or failed its
        # checkout (errors.txt). The rates below are over the rows only, so
        # say which case it is instead of letting the sample shrink unseen.
        seen = [r["commit"] for r in rows]
        stray = sorted(set(seen) - set(listed))
        if stray:
            sys.exit(f"{name}: results for commits outside commits.txt ({stray[:5]}…); another sample's files are mixed in")
        doubled = sorted(c for c, k in collections.Counter(seen).items() if k > 1)
        if doubled:
            sys.exit(f"{name}: several result files for {doubled[:5]}…")
        measured = set(seen)
        missing = [i for i, c in enumerate(listed) if c not in measured]
        failed = (out / "errors.txt").read_text().split("\n") if (out / "errors.txt").exists() else []
        failed = [l for l in failed if l.strip()]
        if failed:
            sys.exit(f"{name}: {len(failed)} checkout(s) still failing in errors.txt; run rerun.sh first")
        if missing and missing != list(range(missing[0], len(listed))):
            sys.exit(f"{name}: commits missing inside the sample (indices {missing[:10]}…); the run is incomplete")
        coverage[name] = (len(rows), len(listed))
        report[name] = rows
    json.dump(report, open(ROOT / "out" / "report.json", "w"), indent=1)

    for name, rows in report.items():
        ok = [r for r in rows if not r.get("error")]
        code = [r for r in ok if r["symbols"] > 0]
        done, listed = coverage[name]
        cut = f", run cut after {done} of {listed} listed" if done < listed else ""
        print(f"\n== {name}: {len(ok)} commits evaluated ({len(rows) - len(ok)} unreadable{cut}), {len(code)} with >=1 changed symbol")
        for label, pop in (("all", ok), ("with symbols", code)):
            if not pop:
                continue
            abst = [r for r in pop if r["motifs"]]
            print(f"  [{label}] abstain {len(abst)}/{len(pop)} = {100 * len(abst) / len(pop):.0f}%")
            present = collections.Counter(m for r in pop for m in r["motifs"])
            sole = collections.Counter(next(iter(r["motifs"])) for r in pop if len(r["motifs"]) == 1)
            for m, n in present.most_common():
                print(f"    {m:32} present {100 * n / len(pop):4.0f}%   sole cause {100 * sole[m] / len(pop):4.0f}%")
            steps = [
                ("docs", {"unsupported_language/docs", "unsupported_language/other:mdc"}),
                ("+ test code", {"outside_symbol/test_code"}),
                ("+ comments/blank", {"outside_symbol/comment_or_blank"}),
                ("+ imports/attributes", {"outside_symbol/import", "outside_symbol/attribute"}),
                ("+ style/assets", {"unsupported_language/style_or_asset"}),
            ]
            excluded = set()
            for label2, add in steps:
                excluded |= add
                left = [r for r in pop if set(r["motifs"]) - excluded]
                print(f"    abstain if {label2:22} no longer downgrade: {100 * len(left) / len(pop):3.0f}%")
            last = collections.Counter(m for r in pop for m in set(r["motifs"]) - excluded)
            print("    what still downgrades then:", ", ".join(f"{m} {100 * n / len(pop):.0f}%" for m, n in last.most_common(6)))


if __name__ == "__main__":
    if len(sys.argv) < 2 or any("=" not in a for a in sys.argv[1:]):
        sys.exit("usage: aggregate.py <name>=<clone> ...  (name as given to run.sh)")
    main(sys.argv[1:])
