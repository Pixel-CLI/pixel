#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Offline scorer: how much of the ORACLE context does a Pixel brief reproduce (issue #906).

The oracle of a census session is what the vanilla agent actually looked at before it answered. It comes from the
``calls`` of ``census.db`` (issue #905), not from a model:

* ``Read`` calls          -> (file, line range) or the whole file (clipped to the 2 000 lines Read returns);
* ``Grep``/``grep``/``rg`` outputs -> the (file, line) hits (and context lines) they printed, files for ``-l``;
* ``sed -n A,Bp`` / ``cat`` / ``head`` / ``tail`` in Bash -> the lines they printed;
* ``git log``/``git show`` outputs -> commit ids and touched files (item recall, no lines);
* ``ls``/``find``/``wc`` outputs -> the files listed (file recall only).

Paths are normalised to repo-relative at the session's pinned sha (``cache/<repo>`` of ``run.py`` is the clone).

A brief is parsed into regions: every ``path:line`` / ``path:a-b`` it prints (``read:``, ``also:``, ``files:``,
``defined:`` and any other label), excerpt blocks headed ``path:a-b`` (numbered or not), and headerless fenced
blocks whose quoted lines are matched back to the lines of the clone. Per session:

* ``line_cov``   oracle lines present in the brief / oracle lines (a search hit counts when its line is present);
* ``inline_cov`` the same, counting only lines the brief QUOTES (excerpt blocks), not pointers to read;
* ``region_cov`` mean over oracle regions (one Read, one grep hit group) of the fraction covered;
* ``file_recall`` oracle files the brief names; ``commit_recall`` oracle commit ids it prints;
* ``answer_cov`` line coverage of the ``path:line`` mentions in the census FINAL ANSWER ("would answer" proxy);
* ``bytes``      the brief size.

CLI::

    score_brief.py --bin <pixel> [--questions Q.jsonl] [--label L] [--only id,id]
    score_brief.py --bin <pixel> --census-db <census.db> [--sessions answer|all]     # the private census
    score_brief.py compare A.json B.json            # side-by-side Markdown of two runs
    score_brief.py freeze-home --commit <rev> --out <dir>   # the probes a binary built at <rev> embeds
    score_brief.py --bin <pixel> --probe-home <dir> ...      # score against that frozen home
    score_brief.py pinned --base <pixel> --head <pixel> --probe-commit <rev>   # both binaries, one frozen home
    score_brief.py --self-test

By default the questions are the public bench (``eval/brief-bench/questions.jsonl``) and the oracle of a question is
its recorded ``oracle`` lines (``question_source.py``); ``--census-db`` scores a local census instead, from its
recorded calls. For each session the repo is cloned (``cp -c`` of ``cache/<repo>``), indexed once per binary and sha
(``<bin> prepare-repo .``, cached under ``score/clones/``) and the brief is built in it: by default as the prompt-submit
hook builds it (``--route hook``: ``<bin> run-hook task-event --provider pi --event prompt-submit`` with a
``brief_only`` payload, the hook's 3 s window and probe floor, all parts in one text), or with ``--route command``
by ``<bin> brief --json "<prompt>"`` (750 ms, no probe floor, one part; plain ``brief`` when the binary has no
``--json``), the route of runs scored before 2026-10-18. Standard library only; run with ``python3 -I``.

The brief runs the probes of ``$HOME/.pixel/plugins``, and the installed ones drift from any commit (two differed from
main on 2026-10-17). ``--probe-home`` points ``HOME`` at a directory written by ``freeze-home`` instead, so two runs
see the same probes; ``report.json`` records which probes ran (``probe_home``), the route the brief took (``route``) and the question file's SHA-256
(``questions``) beside the binary's, and ``compare`` flags two reports whose probes or questions differ (issue #1201).
"""

import argparse
import collections
import hashlib
import importlib.util
import json
import os
import posixpath
import re
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))
import question_source as qs  # noqa: E402

DEFAULT_ROOT = qs.default_root()
CLASSIFY_PATHS = (HERE / "classify.py",)
READ_DEFAULT_LIMIT = 2000   # the Read tool returns at most this many lines when no limit is given
CITE_RANGE_CAP = 400
TYPES = ("definition", "usage", "tests", "config", "docs")
TYPE_LABELS = {"definition": "definition bodies (code read)", "usage": "usage / search-hit lines",
               "tests": "tests", "config": "config", "docs": "docs"}
MEASUREMENT_PROTOCOL = {"name": "brief-oracle", "version": 2}
PROCESS_TIMEOUT_S = 300
COMPLETE_STATUSES = {"ok", "abstained"}


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def classifier():
    """The extractor lane's path rules (config / docs); ``None`` when it is not on this machine."""
    for path in CLASSIFY_PATHS:
        if path.exists():
            try:
                return load("census_classify", path)
            except Exception:  # noqa: BLE001 - a broken optional dependency must not stop the scorer
                continue
    return None


CLASSIFY = classifier()
CONFIG_FALLBACK = (".toml", ".ini", ".cfg", ".conf", ".yaml", ".yml", ".lock", ".json")
DOC_FALLBACK = (".md", ".mdx", ".rst", ".adoc", ".txt")
TEST_RE = re.compile(r"(^|/)(tests?|__tests__|spec|testdata)/|(^|/)test_[^/]*$|_tests?\.[a-z]+$|\.(test|spec)\.[a-z]+$",
                     re.IGNORECASE)


def is_test_path(path):
    return bool(TEST_RE.search(path))


def is_config_path(path):
    if CLASSIFY is not None:
        return CLASSIFY.is_config_path(path)
    return path.lower().endswith(CONFIG_FALLBACK)


def is_doc_path(path):
    if CLASSIFY is not None:
        return CLASSIFY.is_doc_path(path)
    return path.lower().endswith(DOC_FALLBACK)


def line_type(path, kind):
    """Type of an oracle line: path class first (tests, config, docs), else how it was obtained."""
    if is_test_path(path):
        return "tests"
    if is_config_path(path):
        return "config"
    if is_doc_path(path):
        return "docs"
    return "definition" if kind in ("read", "def") else "usage"


# --------------------------------------------------------------------------
# The repository at the pinned sha
# --------------------------------------------------------------------------

WS_PREFIX = None


def add_workspace_root(root):
    """Recorded commands name the session clones as ``<root>/ws/<id>`` (``<root>/work/<id>`` for bench runs), with
    or without macOS's ``/private``: strip those prefixes. Called for the default root and for every ``--root``."""
    global WS_PREFIX
    roots = WS_ROOTS
    for base in (str(root), os.path.realpath(root)):
        for variant in {base, base[len("/private"):] if base.startswith("/private/") else "/private" + base}:
            if variant not in roots:
                roots.append(variant)
    WS_PREFIX = re.compile(r"^(?:" + "|".join(re.escape(r) for r in roots) + r")/(?:ws|work)/[^/]+/?")


WS_ROOTS = []
add_workspace_root(DEFAULT_ROOT)
TEXT_SKIP = (".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".zip", ".gz", ".pdf", ".svg",
             ".snap", ".lock", ".map", ".wasm", ".bin")


def clean_path(path):
    p = str(path or "").strip().strip("'\"`").replace("\\", "/")
    p = WS_PREFIX.sub("", p)
    while p.startswith("./"):
        p = p[2:]
    return p


class Repo:
    """Tracked files, their lines and a (lazy) line-text index of one checkout."""

    def __init__(self, root):
        self.root = Path(root)
        out = subprocess.run(["git", "ls-files", "-z"], cwd=self.root, capture_output=True, check=True)
        self.tracked = {p for p in out.stdout.decode("utf-8", "replace").split("\0") if p}
        self._lines = {}
        self._bases = None
        self._index = None

    def lines(self, rel):
        if rel not in self._lines:
            lines = None
            path = self.root / rel
            try:
                if rel in self.tracked and path.stat().st_size <= 3_000_000:
                    lines = path.read_text(encoding="utf-8", errors="replace").split("\n")
                    if lines and lines[-1] == "":
                        lines.pop()
            except OSError:
                lines = None
            self._lines[rel] = lines
        return self._lines[rel]

    def nlines(self, rel):
        lines = self.lines(rel)
        return len(lines) if lines is not None else 0

    def basenames(self):
        if self._bases is None:
            self._bases = collections.defaultdict(list)
            for p in self.tracked:
                self._bases[posixpath.basename(p)].append(p)
        return self._bases

    def resolve(self, path, bases=("",), suffix=True):
        """Repo-relative tracked path for ``path`` as seen from any of ``bases``, else ``None``."""
        p = clean_path(path)
        if not p or p.startswith("/"):
            return None
        for base in bases:
            cand = posixpath.normpath(posixpath.join(base, p)) if base else posixpath.normpath(p)
            if cand in self.tracked:
                return cand
        if suffix:
            hits = [t for t in self.basenames().get(posixpath.basename(p), ()) if t == p or t.endswith("/" + p)]
            if len(hits) == 1:
                return hits[0]
        return None

    def resolve_dir(self, base, path):
        """Directory (repo-relative, '' for the root) reached by ``cd path`` from ``base``; ``None`` outside."""
        p = clean_path(path)
        if p in ("", "."):
            return ""
        if p.startswith("/"):
            return None
        out = posixpath.normpath(posixpath.join(base, p))
        return None if out.startswith("..") else ("" if out == "." else out)

    def line_index(self):
        """stripped line text -> [(file, line)] for lines of at least 8 characters."""
        if self._index is None:
            index = collections.defaultdict(list)
            for rel in sorted(self.tracked):
                if rel.lower().endswith(TEXT_SKIP):
                    continue
                lines = self.lines(rel)
                if lines is None or len(lines) > 60_000:
                    continue
                for n, text in enumerate(lines, 1):
                    t = text.strip()
                    if len(t) >= 8:
                        index[t].append((rel, n))
            self._index = index
        return self._index


# --------------------------------------------------------------------------
# Oracle
# --------------------------------------------------------------------------

DEF_RE = re.compile(r"(?:^|[\s|(\\^\"'])(?:pub\s+)?(?:async\s+)?(def|fn|class|func|function|struct|enum|trait|impl|"
                    r"interface|const|type)\s")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)(\w+)\1[^\n]*\n.*?\n\s*\2\b", re.DOTALL)
NUM_HIT = re.compile(r"^(?P<p>.+?)(?P<s>[:-])(?P<n>\d+)(?P=s)")
BARE_HIT = re.compile(r"^(?P<n>\d+)(?P<s>[:-])(?P<t>.*)$")
GREP_VERBS = {"grep", "egrep", "fgrep", "rg", "ag", "ack"}
READ_VERBS = {"sed", "cat", "head", "tail", "nl", "bat"}
LIST_VERBS = {"ls", "find", "tree", "wc", "fd", "du"}
TRUNCATORS = {"head", "tail", "cat", "tee", "nl"}
SEPARATORS = {";", "&&", "||", "|", "&"}
HEX = re.compile(r"\b[0-9a-f]{7,40}\b")


class Oracle:
    def __init__(self, repo):
        self.repo = repo
        self.lines = {}          # (file, line) -> type
        self.regions = []        # {file, lo, hi, type, lines: set}
        self.files = {}          # file -> sources
        self.commits = set()     # 7-char prefixes
        self.git_files = set()
        self.list_files = set()

    def add_file(self, rel, source):
        self.files.setdefault(rel, set()).add(source)
        if source == "git":
            self.git_files.add(rel)
        elif source == "list":
            self.list_files.add(rel)

    def add_range(self, rel, lo, hi, kind, source):
        n = self.repo.nlines(rel)
        lo = max(1, lo)
        hi = min(hi, n) if n else hi
        if hi < lo:
            return
        ltype = line_type(rel, kind)
        keys = {(rel, i) for i in range(lo, hi + 1)}
        for key in keys:
            old = self.lines.get(key)
            if old is None or (old == "usage" and ltype == "definition"):
                self.lines[key] = ltype
        self.regions.append({"file": rel, "lo": lo, "hi": hi, "type": ltype, "keys": keys})
        self.add_file(rel, source)

    def add_hit(self, rel, line, kind):
        self.add_range(rel, line, line, kind, "search")


def split_segments(cmd):
    """Shell command -> list of segments ``{argv, sep, writes}``; ``None`` when it cannot be tokenised."""
    text = HEREDOC.sub(" ", cmd).replace("\\\n", " ").replace("\n", " ; ")
    try:
        lex = shlex.shlex(text, posix=True, punctuation_chars=True)
        lex.whitespace_split = True
        lex.commenters = ""
        toks = list(lex)
    except ValueError:
        return None
    segments, argv, writes, i = [], [], False, 0

    def close(sep):
        nonlocal argv, writes
        while argv and "=" in argv[0] and not argv[0].startswith("-") and re.match(r"^\w+=", argv[0]):
            argv = argv[1:]
        while argv and argv[0] in ("rtk", "command", "time", "env", "nohup"):
            argv = argv[1:]
        if argv:
            segments.append({"argv": argv, "sep": sep, "writes": writes})
        argv, writes = [], False

    while i < len(toks):
        tok = toks[i]
        if tok in SEPARATORS:
            close(tok)
        elif tok in ("(", ")", "{", "}"):
            pass
        elif re.fullmatch(r"[<>&]+", tok):
            target = toks[i + 1] if i + 1 < len(toks) else ""
            if argv and re.fullmatch(r"\d", argv[-1]):
                argv.pop()
            if tok.startswith(">") and target not in ("/dev/null",) and not re.fullmatch(r"\d|-", target) \
                    and "&" not in tok:
                writes = True
            i += 1
        else:
            argv.append(tok)
        i += 1
    close(";")
    return segments


def positional(argv):
    return [a for a in argv[1:] if not a.startswith("-") or a == "-"]


def sed_ranges(argv):
    flags = [a for a in argv[1:] if a.startswith("-") and a != "-"]
    quiet = any(re.fullmatch(r"-[a-zA-Z]*n[a-zA-Z]*", f) or f in ("--quiet", "--silent") for f in flags)
    inplace = any(f.startswith("--in-place") or re.fullmatch(r"-[a-zA-Z]*i[a-zA-Z]*", f) for f in flags)
    pos = positional(argv)
    if not quiet or inplace or len(pos) < 2:
        return [], []
    ranges = []
    for m in re.finditer(r"(\d+)(?:,(\d+|\$))?\s*p", pos[0]):
        lo = int(m.group(1))
        hi = m.group(2)
        ranges.append((lo, None if hi == "$" else int(hi) if hi else lo))
    return ranges, pos[1:]


def head_tail_count(argv, default=10):
    for i, a in enumerate(argv[1:], 1):
        if a in ("-n", "--lines") and i + 1 < len(argv):
            return argv[i + 1]
        m = re.fullmatch(r"-n(\+?\d+)", a) or re.fullmatch(r"--lines=(\+?\d+)", a) or re.fullmatch(r"-(\d+)", a)
        if m:
            return m.group(1)
    return str(default)


def pipe_limit(nxt):
    """Line cap when a reader is piped into ``head -N`` (the agent saw only N lines), else ``None``."""
    if nxt is None or posixpath.basename(nxt["argv"][0]) != "head":
        return None
    count = head_tail_count(nxt["argv"])
    return int(count) if count.isdigit() else None


def read_files(argv, repo, bases):
    out = []
    skip = False
    count_arg = head_tail_count(argv)
    for a in argv[1:]:
        if skip:
            skip = False
            continue
        if a in ("-n", "--lines", "-c", "--bytes"):
            skip = True
            continue
        if a.startswith("-") and a != "-":
            continue
        if a == count_arg and re.fullmatch(r"\+?\d+", a):
            continue
        rel = repo.resolve(a, bases)
        if rel:
            out.append(rel)
    return out


def parse_hits(oracle, text, bases, kind, candidates=()):
    """``path:line:text`` / ``path-line-text`` lines (grep -n, rg, the Grep tool) -> oracle hits and files."""
    repo = oracle.repo
    for raw in text.splitlines():
        line = raw.rstrip()
        m = NUM_HIT.match(line)
        if m:
            rel = repo.resolve(m.group("p"), bases, suffix=False)
            if rel:
                oracle.add_hit(rel, int(m.group("n")), kind)
                continue
        b = BARE_HIT.match(line)
        if b and candidates:
            n, snippet = int(b.group("n")), b.group("t").strip()
            chosen = None
            for rel in candidates:
                lines = repo.lines(rel)
                if lines is not None and 1 <= n <= len(lines):
                    have = lines[n - 1].strip()
                    if snippet == have or (snippet and have.startswith(snippet[:30])):
                        chosen = rel
                        break
            if chosen is None and len(candidates) == 1:
                chosen = candidates[0]
            if chosen:
                oracle.add_hit(chosen, n, kind)
                continue
        # files_with_matches / -l output, or `path:count` / unnumbered `path:text`
        head = line.split(":", 1)[0].strip()
        rel = repo.resolve(head, bases, suffix=False)
        if rel:
            oracle.add_file(rel, "search")


def parse_git(oracle, cmd, text, bases):
    repo = oracle.repo
    for m in re.finditer(r"^commit ([0-9a-f]{40})\b", text, re.MULTILINE):
        oracle.commits.add(m.group(1)[:7])
    if re.search(r"--(oneline|pretty|format)", cmd) or re.search(r"\blog\b.*\s-\d+\b|--oneline", cmd):
        for m in re.finditer(r"^([0-9a-f]{7,40})\s+\S", text, re.MULTILINE):
            oracle.commits.add(m.group(1)[:7])
    for m in re.finditer(r"\bgit\s+(?:-C\s+\S+\s+)?(?:show|diff|blame|cat-file(?:\s+-\w)?|log)\b[^;&|]*?\s([0-9a-f]{7,40})\b",
                         cmd):
        oracle.commits.add(m.group(1)[:7])
    names = set(re.findall(r"^diff --git a/(\S+) b/\S+", text, re.MULTILINE))
    names |= set(re.findall(r"^\+\+\+ b/(\S+)", text, re.MULTILINE))
    names |= {m.group(1).strip() for m in re.finditer(r"^\s*(\S[^|\n]*?)\s+\|\s+\d+", text, re.MULTILINE)}
    for raw in text.splitlines():
        s = raw.strip()
        if re.fullmatch(r"[\w@+./-]+\.\w+|[\w@+./-]+/[\w@+./-]+", s) or re.match(r"^[AMDRT]\d*\t(\S+)", s):
            names.add(re.sub(r"^[AMDRT]\d*\t", "", s).split("\t")[-1])
    for name in names:
        rel = repo.resolve(name, bases)
        if rel:
            oracle.add_file(rel, "git")


def parse_listing(oracle, verbs, text, bases):
    repo = oracle.repo
    for raw in text.splitlines():
        s = raw.strip()
        if not s:
            continue
        token = s.split("::", 1)[0].split()[-1] if " " in s else s.split("::", 1)[0]
        rel = repo.resolve(token, bases)
        if rel:
            oracle.add_file(rel, "list")


def bash_oracle(oracle, cmd, text, need=None):
    repo = oracle.repo
    segments = split_segments(cmd)
    if segments is None:
        segments = []
    cwd, cwds = "", [""]
    grep_candidates, saw_grep, saw_git, list_bases, saw_list = [], False, False, [], False
    kind = "def" if need == "find-definition" or DEF_RE.search(cmd) else "use"
    for i, seg in enumerate(segments):
        argv = seg["argv"]
        verb = posixpath.basename(argv[0])
        nxt = segments[i + 1] if i + 1 < len(segments) else None
        piped = seg["sep"] == "|"
        shown = not piped or (nxt is not None and posixpath.basename(nxt["argv"][0]) in TRUNCATORS)
        prev_piped = i > 0 and segments[i - 1]["sep"] == "|"
        bases = tuple(dict.fromkeys([cwd, ""]))
        if verb == "cd" and len(argv) > 1:
            target = repo.resolve_dir(cwd, argv[1])
            if target is not None:
                cwd = target
                if cwd not in cwds:
                    cwds.append(cwd)
            continue
        if seg["writes"] or prev_piped:
            if verb in GREP_VERBS:
                saw_grep = True
            continue
        if verb == "git" and len(argv) > 1:
            sub = next((a for a in argv[1:] if not a.startswith("-") and not repo.resolve(a, bases)), "")
            if sub == "grep":
                saw_grep = True
                grep_candidates += [r for r in (repo.resolve(a, bases) for a in argv[2:]) if r]
            elif sub in ("log", "show", "diff", "blame", "shortlog", "rev-list", "whatchanged", "cat-file"):
                saw_git = True
            elif sub == "ls-files":
                saw_list = True
                list_bases.append(cwd)
        elif verb in GREP_VERBS:
            saw_grep = True
            grep_candidates += [r for r in (repo.resolve(a, bases) for a in argv[1:] if not a.startswith("-")) if r]
        elif verb == "sed" and shown:
            ranges, files = sed_ranges(argv)
            cap = pipe_limit(nxt) if piped else None
            for f in files:
                rel = repo.resolve(f, bases)
                if rel:
                    for lo, hi in ranges:
                        hi = hi if hi is not None else repo.nlines(rel)
                        oracle.add_range(rel, lo, min(hi, lo + cap - 1) if cap and len(ranges) == 1 else hi, "read", "read")
        elif verb in ("cat", "bat", "nl") and shown:
            cap = pipe_limit(nxt) if piped else None
            for rel in read_files(argv, repo, bases):
                oracle.add_range(rel, 1, min(repo.nlines(rel), READ_DEFAULT_LIMIT, cap or READ_DEFAULT_LIMIT), "read", "read")
        elif verb in ("head", "tail") and shown:
            count = head_tail_count(argv)
            for rel in read_files(argv, repo, bases):
                n = repo.nlines(rel)
                k = int(count.lstrip("+") or 10)
                if verb == "head":
                    oracle.add_range(rel, 1, min(k, n), "read", "read")
                elif count.startswith("+"):
                    oracle.add_range(rel, k, n, "read", "read")
                else:
                    oracle.add_range(rel, max(1, n - k + 1), n, "read", "read")
        elif verb in LIST_VERBS or (verb in ("pytest", "python", "python3") and "--collect-only" in argv):
            saw_list = True
            list_bases.append(cwd)
            for a in argv[1:]:
                if not a.startswith("-"):
                    d = repo.resolve_dir(cwd, a)
                    if d is not None:
                        list_bases.append(d)
    if saw_grep:
        parse_hits(oracle, text, tuple(dict.fromkeys(cwds)), kind, tuple(dict.fromkeys(grep_candidates)))
    if saw_git:
        parse_git(oracle, cmd, text, tuple(dict.fromkeys(cwds)))
    if saw_list:
        parse_listing(oracle, LIST_VERBS, text, tuple(dict.fromkeys(list_bases + cwds)))


def oracle_from_ranges(repo, ranges, commits=()):
    """Public oracle ranges (spans are reads, singles are hits), plus explicit reviewed history commit IDs."""
    oracle = Oracle(repo)
    oracle.commits.update(c[:7] for c in commits)
    for rel, lo, hi in ranges:
        if hi - lo >= 2:
            oracle.add_range(rel, lo, hi, "read", "read")
        else:
            oracle.add_range(rel, lo, hi, "use", "search")
    return oracle


def build_oracle(repo, calls):
    """``calls``: dicts with ``tool``, ``input`` (dict), ``output`` (str), ``need``, ``is_error``."""
    oracle = Oracle(repo)
    for call in calls:
        tool, inp, out = call["tool"], call.get("input") or {}, call.get("output") or ""
        if call.get("is_error"):
            continue
        if tool == "Read":
            rel = repo.resolve(inp.get("file_path", ""), suffix=False)
            if not rel:
                continue
            offset = int(inp.get("offset") or 1)
            limit = int(inp.get("limit") or READ_DEFAULT_LIMIT)
            oracle.add_range(rel, offset, offset + limit - 1, "read", "read")
        elif tool == "Grep":
            mode = inp.get("output_mode") or "files_with_matches"
            base = repo.resolve_dir("", inp.get("path", "")) if inp.get("path") else ""
            bases = tuple(dict.fromkeys(["", base or ""]))
            kind = "def" if call.get("need") == "find-definition" or DEF_RE.search(inp.get("pattern", "")) else "use"
            if mode == "content":
                parse_hits(oracle, out, bases, kind)
            else:
                for line in out.splitlines():
                    rel = repo.resolve(line.split(":", 1)[0].strip(), bases, suffix=False)
                    if rel:
                        oracle.add_file(rel, "search")
        elif tool == "Glob":
            for line in out.splitlines():
                rel = repo.resolve(line.strip(), suffix=False)
                if rel:
                    oracle.add_file(rel, "list")
        elif tool == "Bash":
            bash_oracle(oracle, inp.get("command", ""), out, call.get("need"))
    return oracle


# --------------------------------------------------------------------------
# Brief regions
# --------------------------------------------------------------------------

PATH_LINE = re.compile(r"(?<![\w@+./-])([\w@+./-]*[\w@+-]):(\d+)(?:[-–](\d+))?(?![\w])")
FENCE = re.compile(r"^\s*(```+|~~~+)")
LABEL = re.compile(r"^\s*(?:[-*]\s+)?[A-Za-z][\w -]{0,24}:\s")
NUMLINE = re.compile(r"^\s*(\d+)\s*(?:[|:→]|\t)\s?(.*)$")
ELISION = re.compile(r"^\s*(?://\s*|#\s*|/\*\s*)?(\.\.\.|…)\s*(\*/)?\s*$")
HEADERS = (
    re.compile(r"^(?P<p>[\w@+./-]+?):(?P<a>\d+)(?:[-–](?P<b>\d+))?(?:\s*[—–]\s.*|\s+\(.*\))?:?$"),
    re.compile(r"^(?P<p>[\w@+./-]+?)\s*\((?:lines?\s*)?(?P<a>\d+)(?:\s*[-–]\s*(?P<b>\d+))?\)\s*:?$"),
    re.compile(r"^(?P<p>[\w@+./-]+?)#L(?P<a>\d+)(?:-L?(?P<b>\d+))?:?$"),
    re.compile(r"^(?P<p>[\w@+./-]+?\.\w+)(?P<a>)(?P<b>)(?:\s*[—–]\s.*)?:?$"),
)


class BriefRegions:
    def __init__(self):
        self.all = set()      # (file, line) in any region
        self.inline = set()   # (file, line) the brief quotes
        self.files = set()    # files named anywhere
        self.hex = set()      # hex ids (7+ chars) printed
        self.count = 0


def header_of(line, repo):
    """(file, lo, hi) when ``line`` is only an excerpt header, else ``None``."""
    s = line.strip().lstrip("#>*-+ ").strip().strip("`*_ ")
    s = s.rstrip(":").strip("`*_ ")
    if not s or " " in s.split(" —")[0].split(" –")[0].split(" (")[0]:
        return None
    for pat in HEADERS:
        m = pat.match(s)
        if m:
            rel = repo.resolve(m.group("p"))
            if rel:
                a = int(m.group("a")) if m.group("a") else None
                b = int(m.group("b")) if m.group("b") else a
                return rel, a, b
    return None


def matchback(repo, lines, file=None, lo=None, hi=None):
    """Quoted lines -> [(file, line)] found in the clone (the best single file; aligned gaps are filled)."""
    index = repo.line_index()
    cands, votes = [], collections.Counter()
    for k, text in enumerate(lines):
        t = text.strip()
        if len(t) < 8:
            continue
        hits = index.get(t, ())
        if file:
            hits = [h for h in hits if h[0] == file and (lo is None or lo <= h[1] <= (hi or h[1]))]
        if not hits or len(hits) > 40:
            continue
        cands.append((k, hits))
        for f in {h[0] for h in hits}:
            votes[f] += 1
    if not votes:
        return []
    best = file or votes.most_common(1)[0][0]
    if votes[best] * 2 < len(cands) and not file:
        return []
    placed, prev = [], None
    for k, hits in cands:
        pos = sorted(n for f, n in hits if f == best)
        if not pos:
            continue
        pick = next((n for n in pos if prev is not None and n > prev[1] and n - prev[1] <= 4 * max(1, k - prev[0])),
                    pos[0])
        placed.append((k, pick))
        prev = (k, pick)
    out = set()
    for (k1, n1), (k2, n2) in zip(placed, placed[1:]):
        if n2 > n1 and n2 - n1 == k2 - k1:
            out.update(range(n1, n2 + 1))
    out.update(n for _k, n in placed)
    return [(best, n) for n in sorted(out)]


def block_lines(lines, start):
    """Lines of the block that follows a header at ``start``: ``(content, next_index, fenced)``."""
    j = start + 1
    n = len(lines)
    if j < n and FENCE.match(lines[j]):
        mark = FENCE.match(lines[j]).group(1)
        j += 1
        body = []
        while j < n and not lines[j].strip().startswith(mark[:3]):
            body.append(lines[j])
            j += 1
        return body, j + 1, True
    body = []
    while j < n and lines[j].strip() and not LABEL.match(lines[j]) and not lines[j].lstrip().startswith("[PIXEL"):
        body.append(lines[j])
        j += 1
    return body, j, False


def parse_brief(text, repo):
    regions = BriefRegions()
    lines = text.splitlines()
    # 1. every path:line / path:a-b token, wherever it is printed (read:, also:, files:, defined:, tests:, ...)
    for line in lines:
        for m in PATH_LINE.finditer(line):
            rel = repo.resolve(m.group(1), suffix=False)
            if not rel:
                continue
            lo = int(m.group(2))
            hi = int(m.group(3)) if m.group(3) else lo
            if hi < lo or hi - lo > 5000:
                continue
            regions.count += 1
            for n in range(lo, hi + 1):
                regions.all.add((rel, n))
    # 2. excerpt blocks (header + body) and headerless fenced blocks
    i, consumed = 0, set()
    while i < len(lines):
        head = header_of(lines[i], repo) if i not in consumed else None
        if head:
            rel, lo, hi = head
            body, nxt, fenced = block_lines(lines, i)
            if body:
                numbered = [NUMLINE.match(b) for b in body]
                nonblank = [b for b in body if b.strip()]
                have = [m for m in numbered if m]
                got = []
                if nonblank and len(have) * 10 >= len(nonblank) * 6:
                    got = [(rel, int(m.group(1))) for m in have]
                elif lo is not None and not any(ELISION.match(b) for b in body):
                    cap = hi if hi is not None else lo + len(body) - 1
                    sequential = [(rel, n) for n in range(lo, min(lo + len(body) - 1, cap) + 1)]
                    # a fenced block is the brief's own quote of the header range; bare prose after a pointer line
                    # is a block only when its text really sits at that range
                    got = sequential if fenced or matchback(repo, body, file=rel, lo=lo, hi=cap) else []
                else:
                    got = matchback(repo, body, file=rel, lo=lo, hi=hi)
                for key in got:
                    regions.inline.add(key)
                    regions.all.add(key)
                if got:
                    regions.count += 1
                    consumed.update(range(i, nxt))
                    i = nxt
                    continue
        elif FENCE.match(lines[i]) and i not in consumed:
            body, nxt, _fenced = block_lines(lines, i - 1)
            got = matchback(repo, body)
            for key in got:
                regions.inline.add(key)
                regions.all.add(key)
            if got:
                regions.count += 1
            i = nxt
            continue
        i += 1
    # 3. files and commits named anywhere
    for tok in re.findall(r"[\w@+./-]*[\w@+-]\.[A-Za-z][\w+-]*", text):
        rel = repo.resolve(tok)
        if rel:
            regions.files.add(rel)
    regions.files |= {f for f, _n in regions.all}
    regions.hex = set(HEX.findall(text))
    return regions


# --------------------------------------------------------------------------
# Final-answer citations and scoring
# --------------------------------------------------------------------------

CITE = re.compile(r"(?<![\w@+./-])([\w@+./-]*[\w@+-]\.[A-Za-z][\w+-]*):(\d+)(?:[-–](\d+))?")


def cited_lines(answer, repo):
    keys = set()
    for m in CITE.finditer(answer or ""):
        rel = repo.resolve(m.group(1))
        if not rel:
            continue
        lo = int(m.group(2))
        hi = min(int(m.group(3)) if m.group(3) else lo, lo + CITE_RANGE_CAP)
        n = repo.nlines(rel)
        for i in range(lo, hi + 1):
            if not n or i <= n:
                keys.add((rel, i))
    return keys


def ratio(num, den):
    return None if not den else num / den


def score_session(oracle, regions, answer_cites, brief_text):
    total = len(oracle.lines)
    covered = sum(1 for k in oracle.lines if k in regions.all)
    inline = sum(1 for k in oracle.lines if k in regions.inline)
    by_type = {}
    for t in TYPES:
        keys = [k for k, ty in oracle.lines.items() if ty == t]
        by_type[t] = {"lines": len(keys), "covered": sum(1 for k in keys if k in regions.all),
                      "inline": sum(1 for k in keys if k in regions.inline)}
    per_region = [len(r["keys"] & regions.all) / len(r["keys"]) for r in oracle.regions if r["keys"]]
    files = set(oracle.files)
    named = {f for f in files if f in regions.files}
    content = {f for f, src in oracle.files.items() if src - {"list"}}   # looked at, not merely listed
    content_named = content & named
    commits = sorted(oracle.commits)
    commit_hit = [c for c in commits if any(h.startswith(c) or c.startswith(h[:7]) for h in regions.hex)]
    git_items = len(commits) + len(oracle.git_files)
    git_hit = len(commit_hit) + len([f for f in oracle.git_files if f in regions.files])
    list_hit = len([f for f in oracle.list_files if f in regions.files])
    answer_hit = sum(1 for k in answer_cites if k in regions.all)
    return {
        "bytes": len(brief_text.encode("utf-8")),
        "brief_made": bool(brief_text.strip()),
        "oracle_lines": total, "covered_lines": covered, "inline_lines": inline,
        "line_cov": ratio(covered, total), "inline_cov": ratio(inline, total),
        "region_cov": (sum(per_region) / len(per_region)) if per_region else None,
        "oracle_regions": len(per_region),
        "oracle_files": len(files), "files_named": len(named), "file_recall": ratio(len(named), len(files)),
        "content_files": len(content), "content_file_recall": ratio(len(content_named), len(content)),
        "oracle_commits": len(commits), "commits_named": len(commit_hit),
        "commit_recall": ratio(len(commit_hit), len(commits)),
        "cited_lines": len(answer_cites), "cited_covered": answer_hit,
        "answer_cov": ratio(answer_hit, len(answer_cites)),
        "by_type": by_type,
        "git": {"items": git_items, "covered": git_hit},
        "list": {"items": len(oracle.list_files), "covered": list_hit},
        "brief_regions": regions.count,
    }


# --------------------------------------------------------------------------
# Running a binary
# --------------------------------------------------------------------------

def sh(argv, cwd=None, timeout=900, check=True, env=None):
    proc = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)
    if check and proc.returncode != 0:
        raise SystemExit(f"{' '.join(map(str, argv))} (in {cwd}): exit {proc.returncode}\n{proc.stderr[-800:]}")
    return proc


def bin_env(info=None):
    env = dict(os.environ)
    env.pop("ANTHROPIC_API_KEY", None)  # banned everywhere
    env.pop("PIXEL_BRIEF", None)        # the kill switch must not blank a baseline
    if info and info.get("home"):
        env["HOME"] = info["home"]      # the frozen probe home: the brief runs $HOME/.pixel/plugins
    return env


def bin_info(binary):
    real = Path(os.path.realpath(binary))
    digest = hashlib.sha256(real.read_bytes()).hexdigest()
    version = sh([binary, "--version"], check=False).stdout.strip()
    helptext = sh([binary, "brief", "--help"], check=False).stdout
    top = sh([binary, "--help"], check=False).stdout
    return {"path": str(binary), "real_path": str(real), "sha256": digest, "version": version,
            "has_json": "--json" in helptext, "has_meaning": "search-meaning" in top,
            "key": f"{real.name}-{digest[:8]}"}


def activate(info, clone, warm):
    """Make the clone's daemon run with the index and, when ``warm``, the resident chunk vectors loaded.

    A daemon stopped at the end of the previous run would make the next brief take the daemon-less route, which
    answers differently (no meaning search, no history probe), so every run starts it again. The first brief after
    a daemon start also builds what it needs lazily and comes back empty (installed 0.7.1 does, on a large repo),
    so throwaway briefs run until the output settles, before any session is scored. A daemon already serving the
    clone (left by an earlier run, or by a probe's own `pixel` calls outside the harness) is stopped first: a run on
    such a daemon once answered 7 of 56 briefs differently from its replays (an op left unanswered, a probe empty)."""
    sh([info["path"], "daemon", "stop", "."], cwd=clone, check=False, timeout=60, env=bin_env(info))
    try:
        sh([info["path"], "prepare-repo", "."], cwd=clone, timeout=1800, env=bin_env(info))
        if warm:
            if info["has_meaning"]:
                sh([info["path"], "search-meaning", "how does this code work", "."], cwd=clone, timeout=1800,
                   check=False, env=bin_env(info))
            settle(info, clone)
    except BaseException:
        # The caller cannot register a clone whose activation never returned.
        stop_daemons(info, [clone])
        raise


def settle(info, clone, tries=14, need=3):
    """Throwaway briefs until ``need`` in a row print the same text: the daemon finishes building what the first
    briefs ask of it in the background (installed 0.7.1 needs three on excalidraw before it answers)."""
    last, same = None, 0
    for _ in range(tries):
        proc = sh([info["path"], "brief", "--metrics", "off", "where is the main entry point of this program defined",
                   "."], cwd=clone, timeout=600, check=False, env=bin_env(info))
        same = same + 1 if proc.stdout == last else 1
        last = proc.stdout
        if same >= need:
            return
        time.sleep(1)


def indexed_clone(root, info, repo, sha, warm=True):
    """A clone of ``cache/<repo>`` at ``sha`` indexed by this binary; built once per binary, sha and warm state.

    ``warm`` runs one ``search-meaning`` after ``prepare-repo`` (when the binary has it): the chunk vectors that op
    persists under ``.pixel/code-vectors`` are built lazily, and a brief made before they exist is lexical only.
    A hook in a repository the user has already queried sees them, so that is the default state; ``warm=False``
    scores the cold first prompt."""
    base = root / "score" / "clones" / (info["key"] + ("" if warm else "-cold"))
    dest, ready = base / f"{repo}-{sha[:8]}", base / f"{repo}-{sha[:8]}.ready"
    if warm and ready.exists() and dest.exists():
        activate(info, dest, warm)
        return dest
    base.mkdir(parents=True, exist_ok=True)
    if dest.exists():
        shutil.rmtree(dest)
    qs.copy_tree(root / "cache" / repo, dest)
    head = sh(["git", "rev-parse", "HEAD"], cwd=dest).stdout.strip()
    if head != sha:
        raise SystemExit(f"{repo}: clone HEAD {head} is not the session sha {sha}")
    shutil.rmtree(dest / ".pixel", ignore_errors=True)
    activate(info, dest, warm)
    ready.write_text(json.dumps({"binary": info["real_path"], "sha256": info["sha256"], "warm": warm,
                                 "ts": time.time()}))
    return dest


META_KEYS = ("signal", "gate", "kind", "tier", "score", "confidence", "ops", "answered", "error", "elapsed_ms", "cut",
             "deadline_ms", "stages", "probes")
DECISIONS = Path(".pixel") / "brief-decisions.jsonl"


def decision_rows(clone):
    try:
        return (Path(clone) / DECISIONS).read_text(encoding="utf-8").splitlines()
    except OSError:
        return []


def response_text(proc, field, meta):
    """Validate the envelope before trusting context; retain failure evidence separately from credited text."""
    if proc.stderr:
        meta["stderr"] = proc.stderr[-4000:]
    try:
        data = json.loads(proc.stdout)
        if not isinstance(data, dict):
            raise ValueError("response must be an object")
        for key in META_KEYS:
            if key in data:
                meta[key] = data[key]
        text = data.get(field)
        if text is None and (meta.get("gate") == "closed" or meta.get("error") is not None):
            return ""
        if not isinstance(text, str):
            raise ValueError(f"{field} must be a string")
        return text
    except ValueError as err:
        meta.update(malformed=True, response=proc.stdout[-4000:], response_error=str(err))
        return ""


def brief_status(text, meta, code):
    """A status qualifies the run, not answer quality; optional probe outcomes are diagnostic only."""
    if meta.get("timeout"):
        return "timeout"
    if code != 0:
        return "process_error"
    if meta.get("malformed"):
        return "malformed"
    if meta.get("error") is not None:
        return "explicit_error"
    elapsed, deadline = meta.get("elapsed_ms"), meta.get("deadline_ms")
    if meta.get("cut") or (isinstance(elapsed, (int, float)) and isinstance(deadline, (int, float))
                           and elapsed > deadline):
        return "partial"
    return "ok" if text.strip() else "abstained"


def run_hook_brief(info, clone, prompt, session, timeout=PROCESS_TIMEOUT_S):
    """The brief as a prompt-submit hook builds it: Pi's ``brief_only`` payload, which gets the hook's window (3 s),
    its probe floor and the configured parts' budget as one message, without binding a task. The ``brief`` command
    gets 750 ms, no probe floor and one part, so a probe near the edge times out on one run and not the next; this
    route is the live budget, and the decision row the hook appends gives the timing and the gate."""
    payload = json.dumps({"session_id": session, "cwd": str(clone), "prompt": prompt, "brief_only": True})
    env = bin_env(info)
    env["PIXEL_BRIEF_DEDUPE"] = "0"   # every question is a first prompt: nothing was sent to this session before
    before = len(decision_rows(clone))
    started = time.monotonic()
    proc = subprocess.run([info["path"], "run-hook", "task-event", "--provider", "pi", "--event", "prompt-submit"],
                          input=payload, cwd=clone, capture_output=True, text=True, timeout=timeout, env=env)
    elapsed = int((time.monotonic() - started) * 1000)
    rows = decision_rows(clone)
    meta = {}
    if len(rows) > before:
        try:
            row = json.loads(rows[-1])
            if not isinstance(row, dict):
                raise ValueError("decision must be an object")
            meta = {k: row.get(k) for k in META_KEYS if k in row}
        except ValueError as err:
            meta.update(malformed=True, response_error=f"decision row: {err}")
    text = response_text(proc, "context", meta)
    return text, meta, elapsed, proc.returncode


def run_brief(info, clone, prompt, timeout=PROCESS_TIMEOUT_S):
    argv = [info["path"], "brief"] + (["--json"] if info["has_json"] else []) + ["--metrics", "off", prompt, "."]
    started = time.monotonic()
    proc = sh(argv, cwd=clone, timeout=timeout, check=False, env=bin_env(info))
    elapsed = int((time.monotonic() - started) * 1000)
    meta = {}
    if info["has_json"]:
        text = response_text(proc, "brief", meta)
    else:
        text = proc.stdout
        if proc.stderr:
            meta["stderr"] = proc.stderr[-4000:]
    return text, meta, elapsed, proc.returncode


def stop_daemons(info, clones):
    errors = []
    for clone in clones:
        try:
            proc = sh([info["path"], "daemon", "stop", "."], cwd=clone, check=False, timeout=60, env=bin_env(info))
            if proc.returncode:
                errors.append(f"{clone}: stop exit {proc.returncode}: {proc.stderr[-400:]}")
        except (OSError, subprocess.SubprocessError) as err:
            errors.append(f"{clone}: {err}")
    return errors


# --------------------------------------------------------------------------
# The probe home (issue #1201)
# --------------------------------------------------------------------------

BUNDLED_RS = "crates/pixel-setup/src/bundled_probes.rs"
BUNDLED_RE = re.compile(r'include_str!\("\.\./\.\./\.\./probes/([^/"]+)/pixel-plugin\.toml"\)')
PROBE_FILES = ("pixel-plugin.toml", "probe")
PLUGINS_REL = Path(".pixel") / "plugins"
HOME_MANIFEST = "probe-home.json"
# Embedding weights the brief reads and would otherwise download into the fresh home: shared, never written by a run.
SHARED_CACHES = (Path(".local") / "share" / "pixel" / "models",)


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def bundled_names(text):
    """The probe names ``bundled_probes.rs`` embeds, in the order it lists them."""
    return list(dict.fromkeys(BUNDLED_RE.findall(text)))


def freeze_probe_home(repo, rev, out, real_home=None):
    """Write the probes a binary built at ``rev`` embeds into ``<out>/.pixel/plugins``, as ``pixel setup`` would.

    The list and the bytes both come from ``rev`` (``bundled_probes.rs`` and ``probes/<name>/``), not from the working
    tree, so a home frozen at a commit stays that commit's whatever the checkout holds later. ``probe-home.json`` records
    the commit and each file's SHA-256. A directory already frozen at the same commit is reused after a hash check;
    any other non-empty directory is refused."""
    commit = sh(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"], cwd=repo).stdout.strip()
    out = Path(out)
    if (out / HOME_MANIFEST).exists():
        manifest = verify_probe_home(out)
        if manifest["commit"] != commit:
            raise SystemExit(f"{out} holds the probes of {manifest['commit'][:12]}, not {commit[:12]}: pick another --out")
        return manifest
    if out.exists() and any(out.iterdir()):
        raise SystemExit(f"{out} is not empty and holds no {HOME_MANIFEST}: refusing to write a probe home into it")
    names = bundled_names(sh(["git", "show", f"{commit}:{BUNDLED_RS}"], cwd=repo).stdout)
    if not names:
        raise SystemExit(f"{BUNDLED_RS} at {commit[:12]} embeds no probe")
    files = {}
    for name in names:
        dest = out / PLUGINS_REL / name
        dest.mkdir(parents=True)
        for rel in PROBE_FILES:
            data = subprocess.run(["git", "show", f"{commit}:probes/{name}/{rel}"], cwd=repo, capture_output=True,
                                  check=True).stdout
            (dest / rel).write_bytes(data)
            files[f"{name}/{rel}"] = hashlib.sha256(data).hexdigest()
        (dest / "probe").chmod(0o755)
        (dest / ".pixel-bundled").write_text("first-party probe frozen by score_brief.py freeze-home\n")
    real_home = Path(real_home or os.path.expanduser("~"))
    for rel in SHARED_CACHES:
        if (real_home / rel).is_dir():
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).symlink_to(real_home / rel, target_is_directory=True)
    manifest = {"commit": commit, "probes": names, "probe_files": files}
    (out / HOME_MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return manifest


def plugin_files(home):
    """``{"<name>/<file>": sha256}`` of every probe manifest and program under ``<home>/.pixel/plugins``."""
    root = Path(home) / PLUGINS_REL
    if not root.is_dir():
        return {}
    return {f"{d.name}/{rel}": file_sha256(d / rel) for d in sorted(root.iterdir()) if d.is_dir()
            for rel in PROBE_FILES if (d / rel).is_file()}


def verify_probe_home(home):
    """The manifest of a frozen home, after checking its probe files still hash as recorded and nothing switched one
    off (``plugins.toml``): a home edited after freezing would score something no report names."""
    home = Path(home)
    try:
        manifest = json.loads((home / HOME_MANIFEST).read_text(encoding="utf-8"))
    except (OSError, ValueError) as err:
        raise SystemExit(f"{home}: not a frozen probe home ({err}); build one with `score_brief.py freeze-home`")
    have = plugin_files(home)
    if have != manifest.get("probe_files"):
        changed = sorted(set(have.items()) ^ set(manifest.get("probe_files", {}).items()))
        raise SystemExit(f"{home}: probe files differ from {HOME_MANIFEST}: {sorted({k for k, _v in changed})}")
    if (home / ".pixel" / "plugins.toml").exists():
        raise SystemExit(f"{home}: .pixel/plugins.toml exists, so some frozen probe may be disabled; freeze a new home")
    return manifest


def probe_identity(probe_home=None):
    """Which probes a run's briefs saw: the frozen home's commit and hashes, or the installed home, said as such."""
    if probe_home:
        manifest = verify_probe_home(probe_home)
        return {"frozen": True, "home": str(Path(probe_home).resolve()), "commit": manifest["commit"],
                "probe_files": manifest["probe_files"]}
    home = Path(os.path.expanduser("~"))
    toml = home / ".pixel" / "plugins.toml"
    return {"frozen": False, "home": str(home), "commit": None, "probe_files": plugin_files(home),
            "plugins_toml_sha256": file_sha256(toml) if toml.is_file() else None}


def source_identity(source):
    path = Path(source.db_path if source.kind == "census" else source.path)
    return {"kind": source.kind, "path": str(path), "sha256": file_sha256(path)}


def like_for_like(reports, names):
    """Warnings for reports that do not isolate one variable: other questions, an installed or unknown probe home, or
    both the binary and the probes changed (measuring.md: an ablation changes one input)."""
    out = []
    versions = {(r.get("protocol") or {}).get("version", 1) for r in reports}
    if len(versions) > 1:
        out.append("> **Measurement change:** different scorer protocols; these deltas are not retrieval gains.")
    probes = [r.get("probe_home") for r in reports]
    for name, ident, report in zip(names, probes, reports):
        result = report.get("result")
        if result and result["exit_code"]:
            out.append(f"> **{name}: failed run**, coverage is diagnostic only; see its row statuses and errors.")
        if not ident:
            out.append(f"> **{name}** records no probe identity (scored before #1201): its probes are unknown.")
        elif not ident["frozen"]:
            out.append(f"> **{name}** ran the installed probes of `{ident['home']}`, not a frozen home.")
    if len({r.get("route", "command") for r in reports}) > 1:
        out.append("> **Not like for like:** the runs built their briefs through different routes (`--route`).")
    if len({(r.get("questions") or {}).get("sha256") for r in reports}) > 1:
        out.append("> **Not like for like:** the runs scored different question files.")
    probes_differ = len({json.dumps((p or {}).get("probe_files"), sort_keys=True) for p in probes}) > 1
    binaries_differ = len({(r.get("binary") or {}).get("sha256") for r in reports}) > 1
    if probes_differ and binaries_differ:
        out.append("> **Not like for like:** both the binary and the probe files differ between the runs.")
    elif probes_differ:
        out.append("> Same binary, different probe files: the probes are the variable.")
    return out


# --------------------------------------------------------------------------
# Sessions and reports
# --------------------------------------------------------------------------

def load_sessions(db_path, which, only=None):
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    rows = db.execute("select * from sessions where source='census' order by id").fetchall()
    out = []
    for row in rows:
        if which == "answer" and row["mode"] != "answer":
            continue
        if only and row["id"] not in only:
            continue
        calls = []
        for c in db.execute("select seq, tool, input_json, output, need, is_error from calls where session=? order by seq",
                            (row["id"],)):
            try:
                inp = json.loads(c["input_json"] or "{}")
            except ValueError:
                inp = {}
            calls.append({"tool": c["tool"], "input": inp, "output": c["output"] or "", "need": c["need"],
                          "is_error": bool(c["is_error"])})
        out.append({"id": row["id"], "repo": row["repo"], "sha": row["sha"], "category": row["category"],
                    "mode": row["mode"], "prompt": row["prompt"], "final_answer": row["final_answer"] or "",
                    "calls": calls})
    db.close()
    return out


def bench_sessions(source, only=None):
    """The sessions of a bench source in the shape of ``load_sessions``, with the recorded oracle ranges."""
    return [{"id": s["id"], "repo": s["repo"], "sha": s["sha"], "category": s["kind"], "mode": "answer",
             "prompt": s["prompt"], "final_answer": "", "calls": [], "oracle": s["oracle"],
             "oracle_commits": s.get("oracle_commits", [])}
            for s in source.sessions(only)]


def mean(values):
    values = [v for v in values if v is not None]
    return sum(values) / len(values) if values else None


STAGES = ("search_ms", "rank_ms", "render_ms", "probe_ms")


def percentile(values, share):
    """Nearest-rank percentile of ``values`` (``None`` when there are none)."""
    ordered = sorted(values)
    if not ordered:
        return None
    rank = max(1, -(-len(ordered) * share // 100))
    return ordered[int(rank) - 1]


def latency(rows):
    """The brief's own timing, read from ``brief --json``: p95 and max ``elapsed_ms``, how often the window cut the
    packet, and each stage's share of the time. A row whose binary reports no ``elapsed_ms`` is not counted; a stage
    that did not run is absent from its share, not a zero."""
    timed = [m for m in (r.get("brief_meta") or {} for r in rows) if isinstance(m.get("elapsed_ms"), int)]
    elapsed = [m["elapsed_ms"] for m in timed]
    flagged = [m for m in timed if isinstance(m.get("cut"), bool)]
    total = sum(elapsed)
    shares = {}
    for stage in STAGES:
        spent = sum((m.get("stages") or {}).get(stage) or 0 for m in timed)
        shares[stage] = ratio(spent, total) if any((m.get("stages") or {}).get(stage) is not None for m in timed) else None
    return {"n": len(timed), "p95_ms": percentile(elapsed, 95), "max_ms": max(elapsed) if elapsed else None,
            "cut": sum(1 for m in flagged if m["cut"]), "cut_rate": ratio(sum(1 for m in flagged if m["cut"]), len(flagged)),
            "shares": shares}


def aggregate(rows):
    """Macro means over sessions plus pooled (micro) line coverage and per-type pools."""
    agg = {"n": len(rows)}
    for key in ("line_cov", "inline_cov", "region_cov", "file_recall", "content_file_recall", "commit_recall",
                "answer_cov"):
        agg[key] = mean(r["metrics"][key] for r in rows)
    agg["bytes"] = mean(r["metrics"]["bytes"] for r in rows)
    agg["brief_made"] = sum(1 for r in rows if r["metrics"]["brief_made"])
    lines = sum(r["metrics"]["oracle_lines"] for r in rows)
    agg["pooled_line_cov"] = ratio(sum(r["metrics"]["covered_lines"] for r in rows), lines)
    types = {}

    def pool(name, per_session):
        """per_session: [(items, covered, inline)] -> pooled counts and how many sessions it appears in / half covered."""
        types[name] = {"lines": sum(i for i, _c, _q in per_session), "covered": sum(c for _i, c, _q in per_session),
                       "inline": sum(q for _i, _c, q in per_session),
                       "sessions": sum(1 for i, _c, _q in per_session if i),
                       "sessions_half": sum(1 for i, c, _q in per_session if i and 2 * c >= i)}

    for t in TYPES:
        pool(t, [(r["metrics"]["by_type"][t]["lines"], r["metrics"]["by_type"][t]["covered"],
                  r["metrics"]["by_type"][t]["inline"]) for r in rows])
    pool("git", [(r["metrics"]["git"]["items"], r["metrics"]["git"]["covered"], 0) for r in rows])
    pool("list", [(r["metrics"]["list"]["items"], r["metrics"]["list"]["covered"], 0) for r in rows])
    agg["types"] = types
    agg["latency"] = latency(rows)
    return agg


def pct(v):
    return "n/a" if v is None else f"{100 * v:.0f}%"


ROUTES = {"hook": "prompt-submit hook (`run-hook task-event`, Pi `brief_only`: 3 s window, probe floor, all parts)",
          "command": "`brief` command (750 ms window, no probe floor, one part)"}


def render_markdown(report):
    rows = report["sessions"]
    out = [f"# Brief vs oracle: {report['label']}", ""]
    b = report["binary"]
    out.append(f"- measurement protocol: {(report.get('protocol') or {}).get('version', 1)}; "
               f"run status: {(report.get('result') or {}).get('status', 'legacy/unqualified')}")
    out += [f"- binary: `{b['path']}` ({b['version'].splitlines()[0] if b['version'] else '?'}, "
            f"commit {next((l.split(': ')[1] for l in b['version'].splitlines() if l.startswith('commit:')), '?')}, "
            f"sha256 `{b['sha256'][:12]}`, {'brief --json' if b['has_json'] else 'plain brief'})",
            f"- route: {ROUTES.get(report.get('route', 'command'))}",
            f"- sessions: {len(rows)} ({report['sessions_arg']}), scored {report['date']}, "
            f"{'warm index (search-meaning vectors built)' if report.get('warm', True) else 'cold index'}"]
    ident, questions = report.get("probe_home"), report.get("questions")
    if ident:
        out.append(f"- probes: frozen home `{ident['home']}` at commit `{ident['commit'][:12]}` "
                   f"({len(ident['probe_files'])} files)" if ident["frozen"] else
                   f"- probes: **installed home** `{ident['home']}/.pixel/plugins`, not frozen "
                   f"({len(ident['probe_files'])} files)")
    if questions:
        out.append(f"- questions: `{questions['path']}` sha256 `{questions['sha256'][:12]}`")
    result = report.get("result")
    if result:
        out.append(f"- statuses: `{json.dumps(result['counts'], sort_keys=True)}`; exit {result['exit_code']}")
        out += [f"- run error: {err}" for err in result["errors"]]
    out += ["", "| session | category | mode | brief B | oracle lines | line cov | inline cov | region cov | file recall | "
            "answer cov | status | gate |", "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|"]
    for r in rows:
        m = r["metrics"]
        out.append(f"| {r['id']} | {r['category']} | {r['mode']} | {m['bytes']} | {m['oracle_lines']} | "
                   f"{pct(m['line_cov'])} | {pct(m['inline_cov'])} | {pct(m['region_cov'])} | {pct(m['file_recall'])} | "
                   f"{pct(m['answer_cov'])} | {r.get('status', 'legacy/unqualified')} | {r['brief_meta'].get('gate', '-')} |")
    out += ["", "## Means", "", "| group | n | briefs | bytes | line cov | inline cov | region cov | file recall | files read/searched | "
            "answer cov |", "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    groups = [("answer sessions", [r for r in rows if r["mode"] == "answer"]),
              ("edit sessions", [r for r in rows if r["mode"] == "edit"]), ("all", rows)]
    for category in sorted({r["category"] for r in rows}):
        groups.append((f"category {category}", [r for r in rows if r["category"] == category]))
    for name, group in groups:
        if not group:
            continue
        a = aggregate(group)
        out.append(f"| {name} | {a['n']} | {a['brief_made']} | {a['bytes']:.0f} | {pct(a['line_cov'])} | "
                   f"{pct(a['inline_cov'])} | {pct(a['region_cov'])} | {pct(a['file_recall'])} | "
                   f"{pct(a['content_file_recall'])} | {pct(a['answer_cov'])} |")
    out += ["", "## Brief latency (from `brief --json`)", "",
            "| group | timed | p95 ms | max ms | cut | " + " | ".join(f"{s[:-3]} share" for s in STAGES) + " |",
            "|---|---:|---:|---:|---:|" + "---:|" * len(STAGES)]
    for name, group in groups:
        if not group:
            continue
        t = aggregate(group)["latency"]
        cut = "n/a" if t["cut_rate"] is None else f"{t['cut']} ({pct(t['cut_rate'])})"
        out.append(f"| {name} | {t['n']} | {t['p95_ms'] if t['p95_ms'] is not None else 'n/a'} | "
                   f"{t['max_ms'] if t['max_ms'] is not None else 'n/a'} | {cut} | "
                   + " | ".join(pct(t["shares"][s]) for s in STAGES) + " |")
    out += ["", "## Missed oracle regions by type (pooled over all sessions)", "",
            "| type | oracle items | covered | coverage | quoted inline | sessions with it | sessions >= half covered |",
            "|---|---:|---:|---:|---:|---:|---:|"]
    types = aggregate(rows)["types"]
    for t in (*TYPES, "git", "list"):
        v = types[t]
        label = TYPE_LABELS.get(t, {"git": "git (commit ids + touched files)", "list": "listed files"}.get(t))
        out.append(f"| {label} | {v['lines']} | {v['covered']} | {pct(ratio(v['covered'], v['lines']))} | {v['inline']} | "
                   f"{v['sessions']} | {v['sessions_half']} |")
    out.append("")
    return "\n".join(out)


def render_compare(reports, labels):
    names = labels or [r["label"] for r in reports]
    sets = [{r["id"]: r for r in rep["sessions"]} for rep in reports]
    ids = sorted(set().union(*sets))
    cols = ["line cov", "inline", "file recall", "answer cov", "bytes"]
    head = "| session | category | mode | " + " | ".join(f"{n}: {c}" for n in names for c in cols) + " |"
    warnings = like_for_like(reports, names)
    out = (warnings + [""] if warnings else []) + [head, "|" + "---|" * (3 + len(cols) * len(names))]
    for sid in ids:
        first = next(s[sid] for s in sets if sid in s)
        cells = []
        for s in sets:
            m = s[sid]["metrics"] if sid in s else None
            cells += [pct(m["line_cov"]), pct(m["inline_cov"]), pct(m["file_recall"]), pct(m["answer_cov"]),
                      str(m["bytes"])] if m else ["-"] * len(cols)
        out.append(f"| {sid} | {first['category']} | {first['mode']} | " + " | ".join(cells) + " |")
    out += ["", "| group | n | " + " | ".join(f"{n}: {c}" for n in names for c in cols[:4]) + " |",
            "|" + "---|" * (2 + 4 * len(names))]
    cats = sorted({s[sid]["category"] for s in sets for sid in s})
    groups = [("answer sessions", lambda r: r["mode"] == "answer"), ("edit sessions", lambda r: r["mode"] == "edit"),
              ("all", lambda r: True)] + [(f"{c}", (lambda c: lambda r: r["category"] == c)(c)) for c in cats]
    for name, pred in groups:
        cells, n = [], 0
        for rep in reports:
            group = [r for r in rep["sessions"] if pred(r)]
            n = max(n, len(group))
            if group:
                a = aggregate(group)
                cells += [pct(a["line_cov"]), pct(a["inline_cov"]), pct(a["file_recall"]), pct(a["answer_cov"])]
            else:
                cells += ["-"] * 4
        out.append(f"| {name} | {n} | " + " | ".join(cells) + " |")
    out += ["", "| type | " + " | ".join(f"{n}: coverage of oracle items; sessions >= half" for n in names) + " |", "|" + "---|" * (1 + len(names))]
    for t in (*TYPES, "git", "list"):
        cells = []
        for rep in reports:
            v = aggregate(rep["sessions"])["types"][t]
            cells.append(f"{pct(ratio(v['covered'], v['lines']))} of {v['lines']}; {v['sessions_half']}/{v['sessions']} sessions")
        out.append(f"| {TYPE_LABELS.get(t, t)} | " + " | ".join(cells) + " |")
    return "\n".join(out) + "\n"


def score_all(args, root, source):
    info = bin_info(args.bin)
    probes = probe_identity(args.probe_home)
    if probes["frozen"]:
        info["home"] = probes["home"]
    only = set(args.only.split(",")) if args.only else None
    add_workspace_root(root)
    if source.kind == "census":
        sessions = load_sessions(source.db_path, args.sessions, only)
    else:
        sessions = bench_sessions(source, only)
    if not sessions:
        raise SystemExit(f"no {source.kind} sessions selected")
    label = args.label or Path(args.bin).name
    out_dir = Path(args.out or root / "score" / "runs") / label
    (out_dir / "briefs").mkdir(parents=True, exist_ok=True)
    repos, clones, rows, errors = {}, {}, [], []
    try:
        if source.kind != "census":
            source.prepare(root, only)
        for s in sessions:
            key = (s["repo"], s["sha"])
            if s["repo"] not in repos:
                repos[s["repo"]] = Repo(root / "cache" / s["repo"])
            repo = repos[s["repo"]]
            started = time.monotonic()
            try:
                if key not in clones:
                    clones[key] = indexed_clone(root, info, *key, warm=not args.cold)
                if args.route == "hook":
                    text, meta, elapsed, code = run_hook_brief(info, clones[key], s["prompt"],
                                                             f"score-{label}-{s['id']}", timeout=args.timeout)
                else:
                    text, meta, elapsed, code = run_brief(info, clones[key], s["prompt"], timeout=args.timeout)
            except (OSError, subprocess.SubprocessError, SystemExit) as err:
                def decoded(value):
                    return value.decode("utf-8", "replace") if isinstance(value, bytes) else value or ""
                text, code = "", None
                elapsed = int((time.monotonic() - started) * 1000)
                meta = {"error": str(err), "timeout": isinstance(err, subprocess.TimeoutExpired),
                        "response": decoded(getattr(err, "output", ""))[-4000:],
                        "stderr": decoded(getattr(err, "stderr", ""))[-4000:]}
            status = brief_status(text, meta, code)
            (out_dir / "briefs" / f"{s['id']}.txt").write_text(text, encoding="utf-8")
            # Failed text remains on disk as evidence but cannot earn oracle credit.
            scored_text = text if status in ("ok", "partial") else ""
            oracle = (oracle_from_ranges(repo, s["oracle"], s.get("oracle_commits", []))
                      if s.get("oracle") is not None else build_oracle(repo, s["calls"]))
            regions = parse_brief(scored_text, repo) if scored_text.strip() else BriefRegions()
            metrics = score_session(oracle, regions, cited_lines(s["final_answer"], repo), scored_text)
            metrics.update(elapsed_ms=elapsed, exit=code)
            rows.append({"id": s["id"], "repo": s["repo"], "sha": s["sha"], "category": s["category"], "mode": s["mode"],
                         "prompt": s["prompt"], "n_calls": len(s["calls"]), "brief_meta": meta, "metrics": metrics,
                         "status": status, "output_bytes": len(text.encode("utf-8"))})
            print(f"{s['id']:14} status={status:14} bytes={metrics['bytes']:6} line={pct(metrics['line_cov']):>4} "
                  f"inline={pct(metrics['inline_cov']):>4} files={pct(metrics['file_recall']):>4}", flush=True)
    except (Exception, SystemExit, KeyboardInterrupt) as err:
        errors.append(f"{type(err).__name__}: {err}")
    finally:
        incomplete = errors or any(r["status"] not in COMPLETE_STATUSES for r in rows)
        if not args.keep_daemons or incomplete:
            errors.extend(stop_daemons(info, clones.values()))
    failed = bool(errors or len(rows) != len(sessions) or any(r["status"] not in COMPLETE_STATUSES for r in rows))
    report = {"label": label, "binary": info, "route": args.route, "probe_home": probes,
              "protocol": MEASUREMENT_PROTOCOL,
              "scorer": {"sha256": file_sha256(__file__),
                         "commit": sh(["git", "rev-parse", "HEAD"], cwd=qs.REPO).stdout.strip()},
              "result": {"status": "failed" if failed else "complete", "exit_code": int(failed),
                         "counts": dict(collections.Counter(r["status"] for r in rows)), "errors": errors},
              "parameters": {"timeout_s": args.timeout, "selected_ids": [s["id"] for s in sessions],
                             "keep_daemons": args.keep_daemons}, "questions": source_identity(source),
              "sessions_arg": args.sessions, "warm": not args.cold, "date": time.strftime("%Y-%m-%d"),
              "sessions": rows, "means": {"answer": aggregate([r for r in rows if r["mode"] == "answer"]),
                                          "edit": aggregate([r for r in rows if r["mode"] == "edit"]),
                                          "all": aggregate(rows)}}
    (out_dir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    (out_dir / "report.md").write_text(render_markdown(report), encoding="utf-8")
    print(f"wrote {out_dir / 'report.json'} and report.md")
    return out_dir / "report.json"


# --------------------------------------------------------------------------
# self-test
# --------------------------------------------------------------------------

def fixture_repo(base):
    """A tiny git repo: two modules, a test, a doc and a config, with distinctive line texts."""
    repo = Path(base) / "repo"
    files = {
        "src/app.py": [f"    value_{i} = compute({i})  # app line {i}" for i in range(1, 41)],
        "src/util.py": [f"    helper_{i} = merge_values({i})  # util line {i}" for i in range(1, 21)],
        "tests/test_app.py": [f"    assert value_{i} == {i}  # test line {i}" for i in range(1, 16)],
        "README.md": [f"readme sentence number {i} about the app" for i in range(1, 11)],
        "pyproject.toml": [f"key_{i} = 'setting number {i}'" for i in range(1, 6)],
    }
    files["src/app.py"][9] = "def merge(a, b):  # app line 10"
    for rel, lines in files.items():
        path = repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@e", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@e",
           "PATH": os.environ["PATH"], "HOME": str(base)}
    for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "."], ["git", "commit", "-q", "-m", "init"]):
        subprocess.run(argv, cwd=repo, check=True, env=env, capture_output=True)
    sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()
    return repo, sha


FAKE_BIN = '''#!/usr/bin/env python3
import json, sys
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("fakepixel 0.0.1\\ncommit: abc123")
elif a[:2] == ["brief", "--help"]:
    print("Options:\\n      --json   Print the decision")
elif a[:1] == ["brief"]:
    prompt = [x for x in a if not x.startswith("-")][-2]
    text = open(__file__ + ".brief").read() if prompt.startswith("full") else ""
    print(json.dumps({"brief": text, "signal": "weak", "gate": "open" if text else "closed", "elapsed_ms": 120,
                      "cut": False, "deadline_ms": 3000,
                      "stages": {"search_ms": 60, "rank_ms": 40, "render_ms": 2, "probe_ms": None}}))
'''


FAKE_HOME_BIN = '''#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
with open(__file__ + ".calls", "a") as calls:
    calls.write(" ".join(a[:2]) + "\\n")
if a[:1] == ["--version"]:
    print("fakepixel 0.0.2\\ncommit: def456")
elif a[:2] == ["brief", "--help"]:
    print("Options:\\n      --json   Print the decision")
elif a[:4] == ["run-hook", "task-event", "--provider", "pi"]:
    payload = json.load(sys.stdin)
    probe = os.path.join(os.environ["HOME"], ".pixel", "plugins", "fx", "probe")
    text = "read: src/app.py:5-14 - from the fx probe\\n" if os.access(probe, os.X_OK) else ""
    os.makedirs(".pixel", exist_ok=True)
    with open(os.path.join(".pixel", "brief-decisions.jsonl"), "a") as log:
        log.write(json.dumps({"gate": "open" if text else "closed", "deadline_ms": 3000, "elapsed_ms": 42,
                              "kind": f"{payload['brief_only']}-{os.environ.get('PIXEL_BRIEF_DEDUPE')}",
                              "route": "daemon"}) + "\\n")
    print(json.dumps({"decision": "observe", "context": text}))
'''

FAKE_STATUS_BIN = '''#!/usr/bin/env python3
import json, pathlib, sys, time
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("fakepixel 0.0.3")
elif "--help" in a:
    print("--json")
elif a[:1] in (["brief"], ["run-hook"]):
    hook = a[0] == "run-hook"
    mode = json.load(sys.stdin)["prompt"] if hook else a[-2]
    if mode == "timeout":
        time.sleep(2)
    text = "read: src/app.py:5-14\\n" if mode != "abstained" else ""
    meta = {"gate": "closed" if mode == "abstained" else "open"}
    if mode == "explicit_error":
        meta["error"] = "deliberate failure"
    if mode == "partial":
        meta.update(cut=True, elapsed_ms=3001, deadline_ms=3000)
    if mode == "optional_timeout":
        meta["probes"] = [{"name": "optional", "outcome": "timeout"}]
    pathlib.Path(".pixel").mkdir(exist_ok=True)
    with open(".pixel/brief-decisions.jsonl", "a") as log:
        log.write(json.dumps(meta) + "\\n")
    value = dict(meta, **({"context": text} if hook else {"brief": text}))
    if mode == "malformed":
        value = []
    if mode == "wrong_text_type":
        value = {"context" if hook else "brief": 42}
    print(json.dumps(value))
    print("fixture stderr", file=sys.stderr)
    sys.exit(7 if mode == "process_error" else 0)
'''


def probe_repo(base):
    """A pixel-shaped checkout: ``bundled_probes.rs`` embeds ``fx``; a second commit edits it and registers ``late``.
    Returns ``(repo, first, second)``."""
    repo = Path(base) / "pixel-src"
    env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@e", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@e",
           "PATH": os.environ["PATH"], "HOME": str(base), "GIT_CONFIG_GLOBAL": os.devnull}

    def entry(name):
        return (f'    (\n        "{name}",\n        include_str!("../../../probes/{name}/pixel-plugin.toml"),\n'
                f'        include_str!("../../../probes/{name}/probe"),\n    ),\n')

    def commit(names, body, message):
        rs = repo / BUNDLED_RS
        rs.parent.mkdir(parents=True, exist_ok=True)
        rs.write_text("const BUNDLED: &[(&str, &str, &str)] = &[\n" + "".join(map(entry, names)) + "];\n")
        for name in names:
            (repo / "probes" / name).mkdir(parents=True, exist_ok=True)
            (repo / "probes" / name / "pixel-plugin.toml").write_text(f'name = "{name}"\n')
            (repo / "probes" / name / "probe").write_text(body)
        for argv in (["git", "add", "."], ["git", "commit", "-q", "-m", message]):
            subprocess.run(argv, cwd=repo, check=True, env=env, capture_output=True)
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True,
                              text=True).stdout.strip()

    repo.mkdir(parents=True)
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=repo, check=True, env=env, capture_output=True)
    first = commit(["fx"], "#!/bin/sh\necho first\n", "first")
    (repo / "probes" / "unregistered").mkdir(parents=True)
    (repo / "probes" / "unregistered" / "probe").write_text("#!/bin/sh\n")
    second = commit(["fx", "late"], "#!/bin/sh\necho second\n", "second")
    return repo, first, second


class SelfTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.repo_dir, cls.sha = fixture_repo(cls.tmp.name)
        cls.repo = Repo(cls.repo_dir)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def oracle(self, calls):
        return build_oracle(self.repo, [{"tool": t, "input": i, "output": o, "need": n, "is_error": False}
                                        for t, i, o, n in calls])

    # -- tokeniser and path helpers
    def test_split_segments_keeps_sed_script_and_handles_cd_and_redirects(self):
        segs = split_segments("cd /x/y && sed -n '86,92p;312,318p' a.md 2>/dev/null; ls | head -3 > out.txt")
        self.assertEqual([s["argv"][0] for s in segs], ["cd", "sed", "ls", "head"])
        self.assertEqual(sed_ranges(segs[1]["argv"]), ([(86, 92), (312, 318)], ["a.md"]))
        self.assertFalse(segs[1]["writes"])
        self.assertTrue(segs[3]["writes"])

    def test_split_segments_drops_heredoc_bodies(self):
        segs = split_segments("python3 - <<'EOF'\nprint('sed -n 1,2p x')\nEOF\nls")
        self.assertEqual([s["argv"][0] for s in segs], ["python3", "ls"])

    def test_sed_in_place_and_non_quiet_are_not_reads(self):
        self.assertEqual(sed_ranges(["sed", "-i", "", "s/a/b/", "f"])[0], [])
        self.assertEqual(sed_ranges(["sed", "s/a/b/", "f"])[0], [])

    def test_clean_path_strips_workspace_prefix(self):
        self.assertEqual(clean_path(f"{DEFAULT_ROOT}/ws/pixel-4/crates/a.rs"), "crates/a.rs")
        self.assertEqual(clean_path(f"{os.path.realpath(DEFAULT_ROOT)}/work/bench-x-0/crates/a.rs"), "crates/a.rs")
        self.assertEqual(clean_path("./src/x.py"), "src/x.py")
        add_workspace_root("/somewhere/else")
        self.assertEqual(clean_path("/somewhere/else/ws/zc-1-a-brief/src/x.py"), "src/x.py")
        self.assertEqual(clean_path("/private/somewhere/else/ws/zc-1-a-brief/src/x.py"), "src/x.py")

    def test_resolve_exact_suffix_and_dir(self):
        self.assertEqual(self.repo.resolve("src/app.py"), "src/app.py")
        self.assertEqual(self.repo.resolve("app.py"), "src/app.py")
        self.assertEqual(self.repo.resolve("app.py", suffix=False), None)
        self.assertEqual(self.repo.resolve("app.py", bases=("src",), suffix=False), "src/app.py")
        self.assertEqual(self.repo.resolve_dir("", f"{DEFAULT_ROOT}/ws/s-1/src"), "src")

    # -- oracle
    def test_read_ranges_whole_file_and_clipping(self):
        o = self.oracle([("Read", {"file_path": f"{DEFAULT_ROOT}/ws/s-1/src/app.py", "offset": 5,
                                   "limit": 10}, "", None),
                         ("Read", {"file_path": "README.md"}, "", None),
                         ("Read", {"file_path": "src/util.py", "offset": 18, "limit": 50}, "", None)])
        self.assertEqual({k for k in o.lines if k[0] == "src/app.py"}, {("src/app.py", i) for i in range(5, 15)})
        self.assertEqual(sum(1 for k in o.lines if k[0] == "README.md"), 10)
        self.assertEqual({k[1] for k in o.lines if k[0] == "src/util.py"}, {18, 19, 20})
        self.assertEqual(o.lines[("README.md", 3)], "docs")
        self.assertEqual(o.lines[("src/app.py", 5)], "definition")

    def test_grep_tool_content_hits_context_files_and_types(self):
        out = "src/app.py:10:def merge(a, b):\nsrc/util.py-2-    helper_2\nsrc/util.py:3:merge\ntests/test_app.py:4:x\n--"
        o = self.oracle([("Grep", {"pattern": "def merge", "output_mode": "content"}, out, None),
                         ("Grep", {"pattern": "merge", "output_mode": "files_with_matches"},
                          "Found 2 files\nsrc/app.py\nREADME.md", "find-usages")])
        self.assertEqual(o.lines[("src/app.py", 10)], "definition")   # a definition pattern
        self.assertEqual(o.lines[("tests/test_app.py", 4)], "tests")
        self.assertIn(("src/util.py", 2), o.lines)                    # context lines were printed too
        self.assertEqual(set(o.files), {"src/app.py", "src/util.py", "tests/test_app.py", "README.md"})
        self.assertNotIn(("README.md", 1), o.lines)                   # file-level only

    def test_bash_sed_cat_head_tail_and_bare_grep_numbers(self):
        cmd = (f"cd {DEFAULT_ROOT}/ws/s-1; sed -n '20,24p' src/app.py && head -3 README.md "
               "&& tail -n 2 pyproject.toml; grep -n value_3 src/util.py; cat src/util.py | head -2")
        o = self.oracle([("Bash", {"command": cmd}, "3:    helper_3 = merge_values(3)  # util line 3\n", None)])
        keys = set(o.lines)
        self.assertTrue({("src/app.py", n) for n in range(20, 25)} <= keys)
        self.assertEqual({k[1] for k in keys if k[0] == "README.md"}, {1, 2, 3})
        self.assertEqual({k[1] for k in keys if k[0] == "pyproject.toml"}, {4, 5})
        self.assertIn(("src/util.py", 3), keys)         # the bare `3:text` line, verified against the file
        self.assertEqual(o.lines[("pyproject.toml", 5)], "config")

    def test_piped_reads_that_are_filtered_are_not_oracle(self):
        o = self.oracle([("Bash", {"command": "sed -n '1,5p' src/app.py | grep zzz"}, "", None)])
        self.assertEqual(o.lines, {})

    def test_grep_with_directory_cwd_and_multi_file_output(self):
        out = "src/app.py:3:    value_3 = compute(3)  # app line 3\nsrc/util.py:3:z\n"
        o = self.oracle([("Bash", {"command": "cd src; grep -rn 'value_3' . | head -5"}, out, None)])
        self.assertEqual(set(o.lines), {("src/app.py", 3), ("src/util.py", 3)})

    def test_git_show_listing_and_collect_only(self):
        commit = "a" * 40
        show = f"commit {commit}\nAuthor: t\n\n    init\n\n src/app.py   | 4 ++--\n README.md | 1 +\n 2 files changed\n"
        o = self.oracle([("Bash", {"command": "git show --stat HEAD"}, show, None),
                         ("Bash", {"command": "git log --oneline -3"}, "bbbbbbb fix thing\nccccccc other\n", None),
                         ("Bash", {"command": "ls src && wc -l src/*.py"}, "app.py\nutil.py\n  40 src/app.py\n", None)])
        self.assertEqual(o.commits, {"aaaaaaa", "bbbbbbb", "ccccccc"})
        self.assertEqual(o.git_files, {"src/app.py", "README.md"})
        self.assertEqual(o.list_files, {"src/app.py", "src/util.py"})
        self.assertEqual(o.lines, {})
        m = score_session(o, BriefRegions(), set(), "")
        self.assertEqual((m["oracle_files"], m["content_files"]), (3, 2))   # util.py was only listed

    def test_errors_and_untracked_files_are_skipped(self):
        o = Oracle(self.repo)
        o2 = build_oracle(self.repo, [{"tool": "Read", "input": {"file_path": "nope.py"}, "output": "", "need": None,
                                       "is_error": False},
                                      {"tool": "Read", "input": {"file_path": "src/app.py"}, "output": "",
                                       "need": None, "is_error": True}])
        self.assertEqual((o.lines, o2.lines), ({}, {}))

    # -- brief parsing
    def test_brief_labels_files_items_and_defined(self):
        text = ("[PIXEL:BRIEF]\nkind: x\nfiles: src/app.py:10 — def merge; src/util.py:3 — merge(; "
                "README.md:2 — text (+1 more)\ndefined: function merge src/app.py:10-12 — def merge(\n"
                "read: src/app.py:5-9 — head\nalso: tests/test_app.py:1-3\nRead these ranges\n")
        r = parse_brief(text, self.repo)
        self.assertTrue({("src/app.py", n) for n in (5, 6, 7, 8, 9, 10, 11, 12)} <= r.all)
        self.assertIn(("src/util.py", 3), r.all)
        self.assertIn(("README.md", 2), r.all)
        self.assertNotIn(("src/util.py", 4), r.all)
        self.assertEqual(r.inline, set())
        self.assertTrue({"src/app.py", "src/util.py", "tests/test_app.py"} <= r.files)

    def test_brief_header_blocks_numbered_unnumbered_and_fenced(self):
        quoted = "\n".join(self.repo.lines("src/util.py")[4:7])
        text = (f"### src/util.py:5-7\n```\n{quoted}\n```\n\n"
                "src/app.py:30-31\n    value_30 = compute(30)  # app line 30\n    value_31 = compute(31)  # app line 31\n"
                "\n  tests/test_app.py:8-9\n  8| assert\n  9| assert\n")
        r = parse_brief(text, self.repo)
        self.assertTrue({("src/util.py", n) for n in (5, 6, 7)} <= r.inline)
        self.assertEqual({k for k in r.inline if k[0] == "src/app.py"}, {("src/app.py", 30), ("src/app.py", 31)})
        self.assertEqual({k for k in r.inline if k[0] == "tests/test_app.py"},
                         {("tests/test_app.py", 8), ("tests/test_app.py", 9)})

    def test_brief_headerless_fenced_block_is_matched_back(self):
        quoted = "\n".join(self.repo.lines("src/app.py")[19:24])
        r = parse_brief(f"answer pack\n```\n{quoted}\n```\n", self.repo)
        self.assertEqual({k[1] for k in r.inline if k[0] == "src/app.py"}, set(range(20, 25)))

    def test_brief_elided_header_block_uses_matchback_not_the_header_range(self):
        lines = self.repo.lines("src/app.py")
        body = "\n".join([lines[1], lines[2], "    ...", lines[30], lines[31]])
        r = parse_brief(f"src/app.py:2-40\n```\n{body}\n```\n", self.repo)
        got = {k[1] for k in r.inline}
        self.assertTrue({2, 3, 31, 32} <= got)
        self.assertNotIn(10, got)

    def test_brief_unrelated_tokens_and_unknown_files_are_ignored(self):
        r = parse_brief("searched: 12:30 for x.y:3 nofile.rs:4-9 and version 1.2:5\n", self.repo)
        self.assertEqual(r.all, set())

    # -- citations and scoring
    def test_cited_lines_from_final_answer(self):
        keys = cited_lines("See `src/app.py:10`, src/util.py:3-5 and README.md:99 (past the end) and zzz.py:1", self.repo)
        self.assertEqual({k for k in keys if k[0] == "src/util.py"}, {("src/util.py", n) for n in (3, 4, 5)})
        self.assertIn(("src/app.py", 10), keys)
        self.assertNotIn(("README.md", 99), keys)

    def test_score_session_numbers(self):
        o = self.oracle([
            ("Read", {"file_path": "src/app.py", "offset": 5, "limit": 10}, "", None),
            ("Grep", {"pattern": "merge", "output_mode": "content"},
             "src/app.py:10:def merge\nsrc/util.py:3:merge\nsrc/util.py-4-x\n", "find-usages"),
            ("Bash", {"command": "sed -n '20,24p' src/app.py"}, "", None),
            ("Read", {"file_path": "README.md"}, "", None),
            ("Read", {"file_path": "tests/test_app.py", "offset": 3, "limit": 1}, "", None),
            ("Read", {"file_path": "pyproject.toml"}, "", None),
            ("Bash", {"command": "git show --stat HEAD"}, f"commit {self.sha}\n\n src/app.py | 1 +\n", None),
        ])
        self.assertEqual(len(o.lines), 33)
        quoted = "\n".join(self.repo.lines("src/util.py")[2:4])
        text = ("files: src/app.py:10 — def merge; src/util.py:3 — merge(\nread: src/app.py:5-9 — head\n"
                f"also: src/app.py:20-22\nsrc/util.py:3-4\n{quoted}\ncommit {self.sha[:9]}\n")
        r = parse_brief(text, self.repo)
        m = score_session(o, r, cited_lines("see src/app.py:10 and src/app.py:30", self.repo), text)
        self.assertEqual((m["oracle_lines"], m["covered_lines"], m["inline_lines"]), (33, 11, 2))
        self.assertAlmostEqual(m["line_cov"], 11 / 33)
        self.assertEqual(m["by_type"]["definition"], {"lines": 15, "covered": 9, "inline": 0})
        self.assertEqual(m["by_type"]["usage"], {"lines": 2, "covered": 2, "inline": 2})
        self.assertEqual(m["by_type"]["docs"]["covered"], 0)
        self.assertEqual(m["files_named"], 2)
        self.assertEqual(m["oracle_files"], 5)
        self.assertEqual((m["oracle_commits"], m["commits_named"]), (1, 1))
        self.assertEqual((m["cited_lines"], m["cited_covered"]), (2, 1))
        self.assertEqual(m["git"], {"items": 2, "covered": 2})
        self.assertGreater(m["region_cov"], 0)
        self.assertLess(m["region_cov"], 1)

    def test_empty_brief_scores_zero_and_empty_oracle_is_none(self):
        o = self.oracle([("Read", {"file_path": "README.md"}, "", None)])
        m = score_session(o, BriefRegions(), set(), "")
        self.assertEqual((m["line_cov"], m["brief_made"], m["bytes"], m["answer_cov"]), (0.0, False, 0, None))
        m2 = score_session(self.oracle([]), BriefRegions(), set(), "")
        self.assertIsNone(m2["line_cov"])
        self.assertIsNone(m2["file_recall"])

    def test_aggregate_means_skip_none_and_pool_lines(self):
        def row(cov, total, covered, answer):
            m = score_session(self.oracle([]), BriefRegions(), set(), "")
            m.update({"line_cov": cov, "oracle_lines": total, "covered_lines": covered, "answer_cov": answer})
            return {"metrics": m}
        agg = aggregate([row(0.5, 10, 5, None), row(1.0, 30, 30, 0.2), row(None, 0, 0, None)])
        self.assertAlmostEqual(agg["line_cov"], 0.75)
        self.assertAlmostEqual(agg["answer_cov"], 0.2)
        self.assertIsNone(agg["content_file_recall"])
        self.assertAlmostEqual(agg["pooled_line_cov"], 35 / 40)

    def test_latency_reads_the_brief_json_timing_and_skips_untimed_rows(self):
        def row(meta):
            return {"brief_meta": meta}
        rows = [row({"elapsed_ms": ms, "cut": ms >= 3000,
                     "stages": {"search_ms": ms // 2, "rank_ms": ms // 4, "render_ms": 1, "probe_ms": None}})
                for ms in (100, 200, 300, 400, 500, 600, 700, 800, 900, 3000)]
        rows.append(row({"gate": "open"}))                     # an older binary: no timing, not a zero
        t = latency(rows)
        self.assertEqual(t["n"], 10)
        self.assertEqual(t["p95_ms"], 3000)
        self.assertEqual(t["max_ms"], 3000)
        self.assertEqual((t["cut"], t["cut_rate"]), (1, 0.1))
        self.assertAlmostEqual(t["shares"]["search_ms"], 3750 / 7500)
        self.assertIsNone(t["shares"]["probe_ms"])
        self.assertEqual(percentile([5, 1, 3], 50), 3)
        self.assertIsNone(latency([row({})])["p95_ms"])

    # -- the CLI end to end with a fake binary, a fake census.db and a fake clone cache
    def status_fixture(self, base, modes):
        binary, questions = base / "fakepixel", base / "questions.jsonl"
        binary.write_text(FAKE_STATUS_BIN)
        binary.chmod(0o755)
        questions.write_text("".join(json.dumps({"id": mode, "repo": "o/fx", "url": str(self.repo_dir),
                            "sha": self.sha, "prompt": mode, "kind": "lookup",
                            "oracle": [["src/app.py", 5, 14]]}) + "\n" for mode in modes))
        return ["--bin", str(binary), "--questions", str(questions), "--root", str(base / "state"),
                "--out", str(base / "runs"), "--cold"], base / "runs"

    def test_invalid_outputs_cannot_be_successful_comparisons_on_either_route(self):
        modes = ["ok", "abstained", "process_error", "malformed", "wrong_text_type", "explicit_error", "partial",
                 "optional_timeout"]
        for route in ("hook", "command"):
            with self.subTest(route=route), tempfile.TemporaryDirectory() as tmp:
                common, runs = self.status_fixture(Path(tmp), modes)
                self.assertEqual(main(common + ["--route", route, "--label", "faults"]), 1)
                report = json.loads((runs / "faults/report.json").read_text())
                rows = {r["id"]: r for r in report["sessions"]}
                self.assertEqual(report["result"]["status"], "failed")
                self.assertEqual(report["means"]["all"]["n"], len(modes))
                for mode in modes:
                    expected = "ok" if mode == "optional_timeout" else "malformed" if mode == "wrong_text_type" else mode
                    self.assertEqual(rows[mode]["status"], expected)
                    cov = 1.0 if mode in ("ok", "partial", "optional_timeout") else 0.0
                    self.assertEqual(rows[mode]["metrics"]["line_cov"], cov)
                self.assertEqual(rows["process_error"]["metrics"]["exit"], 7)
                self.assertIn("fixture stderr", rows["process_error"]["brief_meta"]["stderr"])
                self.assertEqual(main(["compare", str(runs / "faults/report.json")]), 1)
                # Abstention remains a zero in the denominator; an optional probe timeout is not a brief failure.
                selected = ",".join(("ok", "abstained", "optional_timeout"))
                self.assertEqual(main(common + ["--route", route, "--label", "healthy", "--only", selected]), 0)
                healthy = json.loads((runs / "healthy/report.json").read_text())
                self.assertEqual(healthy["means"]["all"]["line_cov"], 2 / 3)

    def test_nonzero_json_process_fails_even_with_full_context(self):
        with tempfile.TemporaryDirectory() as tmp:
            common, runs = self.status_fixture(Path(tmp), ["process_error"])
            self.assertEqual(main(common + ["--label", "failed"]), 1)
            row = json.loads((runs / "failed/report.json").read_text())["sessions"][0]
            self.assertEqual((row["metrics"]["exit"], row["metrics"]["line_cov"]), (7, 0.0))

    def test_pinned_saves_both_failed_reports_and_propagates_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            common, runs = self.status_fixture(base, ["process_error"])
            repo, commit, _ = probe_repo(base / "source")
            code = main(["pinned", "--base", common[1], "--repo", str(repo), "--probe-commit", commit,
                         "--label", "pair"] + common[2:])
            self.assertEqual(code, 1)
            for side in ("base", "head"):
                report = json.loads((runs / f"pair-{side}/report.json").read_text())
                self.assertEqual(report["result"]["exit_code"], 1)
            self.assertIn("failed run", (runs / "pair-compare.md").read_text())

    def test_process_timeout_is_reported_on_both_routes(self):
        for route in ("hook", "command"):
            with self.subTest(route=route), tempfile.TemporaryDirectory() as tmp:
                common, runs = self.status_fixture(Path(tmp), ["timeout"])
                self.assertEqual(main(common + ["--route", route, "--timeout", "0.05", "--label", "slow"]), 1)
                row = json.loads((runs / "slow/report.json").read_text())["sessions"][0]
                self.assertEqual((row["status"], row["metrics"]["exit"], row["metrics"]["line_cov"]),
                                 ("timeout", None, 0.0))

    def test_activation_failure_stops_the_unregistered_clone(self):
        info = {"path": "fake"}
        with mock.patch(f"{__name__}.sh", side_effect=[subprocess.CompletedProcess([], 0), OSError("prepare")]), \
             mock.patch(f"{__name__}.stop_daemons", return_value=[]) as cleanup:
            with self.assertRaisesRegex(OSError, "prepare"):
                activate(info, self.repo_dir, False)
        cleanup.assert_called_once_with(info, [self.repo_dir])

    def test_timeout_and_prepare_failure_leave_reports_and_cleanup(self):
        for phase in ("brief", "prepare"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as tmp:
                common, runs = self.status_fixture(Path(tmp), ["ok"])
                target = "run_hook_brief" if phase == "brief" else "activate"
                fault = subprocess.TimeoutExpired(["fakepixel"], 0.01, output=b"partial", stderr=b"deadline")
                with mock.patch(f"{__name__}.{target}", side_effect=fault), \
                     mock.patch(f"{__name__}.stop_daemons", return_value=[]) as cleanup:
                    self.assertEqual(main(common + ["--label", "timeout"]), 1)
                report = json.loads((runs / "timeout/report.json").read_text())
                self.assertEqual(report["result"]["status"], "failed")
                self.assertEqual(report["sessions"][0]["status"], "timeout")
                self.assertEqual(report["sessions"][0]["metrics"]["line_cov"], 0.0)
                self.assertIn("deadline", report["sessions"][0]["brief_meta"]["stderr"])
                self.assertTrue(cleanup.called)

    def test_cli_end_to_end(self):
        root = Path(self.tmp.name) / "census"
        (root / "cache").mkdir(parents=True)
        shutil.copytree(self.repo_dir, root / "cache" / "fx", symlinks=True)
        binary = Path(self.tmp.name) / "fakepixel"
        binary.write_text(FAKE_BIN)
        binary.chmod(0o755)
        brief = ("files: src/app.py:10 — def merge\nread: src/app.py:5-14 — merge\n")
        Path(str(binary) + ".brief").write_text(brief)
        db = sqlite3.connect(root / "census.db")
        db.executescript("""create table sessions(id text primary key, source text, repo text, sha text, category text,
            mode text, prompt text, prompt_hash text, started_at text, wall_s real, model text, tokens_in integer,
            tokens_out integer, cost_usd real, final_answer text, ok integer, raw_path text);
            create table calls(session text, seq integer, tool text, input_json text, output text, output_bytes integer,
            is_error integer, class text, need text, ts text, turn integer, call_id text);""")
        rows = [("fx-1", "answer", "full question", "answer at src/app.py:10"), ("fx-2", "edit", "other", "done")]
        for sid, mode, prompt, answer in rows:
            db.execute("insert into sessions(id,source,repo,sha,category,mode,prompt,final_answer) values(?,?,?,?,?,?,?,?)",
                       (sid, "census", "fx", self.sha, "lookup", mode, prompt, answer))
            db.execute("insert into calls(session,seq,tool,input_json,output,is_error,need) values(?,?,?,?,?,?,?)",
                       (sid, 0, "Read", json.dumps({"file_path": "src/app.py", "offset": 5, "limit": 10}), "", 0, None))
        db.commit()
        db.close()
        code = main(["--root", str(root), "--census-db", str(root / "census.db"), "--bin", str(binary), "--sessions", "all",
                     "--label", "fake", "--route", "command"])
        self.assertEqual(code, 0)
        report = json.loads((root / "score" / "runs" / "fake" / "report.json").read_text())
        by_id = {r["id"]: r for r in report["sessions"]}
        self.assertEqual(by_id["fx-1"]["metrics"]["line_cov"], 1.0)
        self.assertEqual(by_id["fx-1"]["metrics"]["answer_cov"], 1.0)
        self.assertEqual(by_id["fx-1"]["brief_meta"]["gate"], "open")
        self.assertEqual(by_id["fx-2"]["metrics"]["line_cov"], 0.0)
        self.assertFalse(by_id["fx-2"]["metrics"]["brief_made"])
        self.assertEqual(report["means"]["answer"]["n"], 1)
        md = (root / "score" / "runs" / "fake" / "report.md").read_text()
        self.assertIn("| fx-1 | lookup | answer |", md)
        self.assertIn("## Missed oracle regions by type", md)
        self.assertEqual(report["means"]["all"]["latency"]["p95_ms"], 120)
        self.assertIn("| all | 2 | 120 | 120 | 0 (0%) | 50% | 33% | 2% | n/a |", md)
        self.assertIn("fx-1", render_compare([report, report], ["a", "b"]))
        # a second run reuses the indexed clone (the .ready marker) and only-filters
        code = main(["--root", str(root), "--census-db", str(root / "census.db"), "--bin", str(binary),
                     "--sessions", "answer", "--label", "fake2", "--only", "fx-1", "--route", "command"])
        self.assertEqual(code, 0)
        self.assertEqual(len(json.loads((root / "score" / "runs" / "fake2" / "report.json").read_text())["sessions"]), 1)

    def test_cli_scores_the_public_bench_by_default_from_the_recorded_oracle(self):
        root = Path(self.tmp.name) / "bench-root"
        bench = Path(self.tmp.name) / "bench"
        bench.mkdir(exist_ok=True)
        question = {"id": "fx-01", "repo": "o/fx", "url": str(self.repo_dir), "sha": self.sha, "prompt": "full question",
                    "kind": "lookup", "oracle": [["src/app.py", 5, 14], ["README.md", 2, 2]], "oracle_lines": 11,
                    "oracle_commits": [self.sha]}
        (bench / "questions.jsonl").write_text(json.dumps(question) + "\n")
        binary = Path(self.tmp.name) / "fakepixel-bench"
        binary.write_text(FAKE_BIN)
        binary.chmod(0o755)
        Path(str(binary) + ".brief").write_text(f"read: src/app.py:5-14 — merge\ngit: {self.sha}\n")
        code = main(["--root", str(root), "--questions", str(bench / "questions.jsonl"), "--bin", str(binary),
                     "--label", "bench", "--route", "command"])
        self.assertEqual(code, 0)
        report = json.loads((root / "score" / "runs" / "bench" / "report.json").read_text())
        row = report["sessions"][0]
        self.assertEqual((row["id"], row["category"], row["mode"]), ("fx-01", "lookup", "answer"))
        self.assertEqual(row["metrics"]["oracle_lines"], 11)
        self.assertEqual(row["metrics"]["covered_lines"], 10)    # the span is quoted, the single README hit is not
        self.assertEqual(row["metrics"]["oracle_commits"], 1)
        self.assertEqual(row["metrics"]["commit_recall"], 1.0)
        self.assertTrue((root / "cache" / "o__fx" / ".git").exists())

    # -- the frozen probe home (#1201)
    def test_bundled_names_parse_the_real_registry(self):
        # A reformatted bundled_probes.rs must not silently freeze an empty or partial home.
        text = (qs.REPO / BUNDLED_RS).read_text(encoding="utf-8")
        names = bundled_names(text)
        self.assertEqual(len(names), text.count("include_str!(") // 2)
        for name in names:
            for rel in PROBE_FILES:
                self.assertTrue((qs.REPO / "probes" / name / rel).is_file(), f"{name}/{rel}")

    def test_freeze_home_writes_the_commit_not_the_working_tree(self):
        base = Path(self.tmp.name) / "freeze"
        repo, first, second = probe_repo(base)
        (repo / "probes" / "fx" / "probe").write_text("#!/bin/sh\necho uncommitted\n")
        real = base / "real-home"
        (real / SHARED_CACHES[0]).mkdir(parents=True)
        home = base / "home-first"
        manifest = freeze_probe_home(repo, first, home, real_home=real)
        self.assertEqual((manifest["commit"], manifest["probes"]), (first, ["fx"]))
        program = home / PLUGINS_REL / "fx" / "probe"
        self.assertEqual(program.read_text(), "#!/bin/sh\necho first\n")
        self.assertTrue(os.access(program, os.X_OK))
        self.assertFalse((home / PLUGINS_REL / "late").exists())
        self.assertFalse((home / PLUGINS_REL / "unregistered").exists())
        self.assertEqual(manifest["probe_files"]["fx/probe"],
                         hashlib.sha256(b"#!/bin/sh\necho first\n").hexdigest())
        self.assertEqual(json.loads((home / HOME_MANIFEST).read_text()), manifest)
        self.assertEqual(os.path.realpath(home / SHARED_CACHES[0]), os.path.realpath(real / SHARED_CACHES[0]))
        self.assertEqual(freeze_probe_home(repo, "HEAD~1", home, real_home=real), manifest)  # same commit: reused
        with self.assertRaises(SystemExit):
            freeze_probe_home(repo, second, home, real_home=real)                           # another commit: refused
        stray = base / "not-a-home"
        stray.mkdir()
        (stray / "file").write_text("x")
        with self.assertRaises(SystemExit):
            freeze_probe_home(repo, first, stray, real_home=real)
        self.assertEqual(freeze_probe_home(repo, second, base / "home-second", real_home=real)["probes"], ["fx", "late"])

    def test_a_frozen_home_edited_after_freezing_is_refused(self):
        base = Path(self.tmp.name) / "edited"
        repo, first, _second = probe_repo(base)
        home = base / "home"
        freeze_probe_home(repo, first, home, real_home=base)
        self.assertTrue(probe_identity(home)["frozen"])
        (home / PLUGINS_REL / "fx" / "probe").write_text("#!/bin/sh\necho edited\n")
        with self.assertRaises(SystemExit):
            probe_identity(home)
        (home / PLUGINS_REL / "fx" / "probe").write_text("#!/bin/sh\necho first\n")
        (home / ".pixel" / "plugins.toml").write_text("[fx]\nenabled = false\n")
        with self.assertRaises(SystemExit):
            probe_identity(home)

    def test_the_brief_runs_the_frozen_probes_and_the_report_names_them(self):
        base = Path(self.tmp.name) / "pinned"
        repo, first, second = probe_repo(base)
        installed = base / "installed-home"            # the machine's home: no fx probe installed
        (installed / PLUGINS_REL / "other").mkdir(parents=True)
        (installed / PLUGINS_REL / "other" / "probe").write_text("#!/bin/sh\n")
        root, bench = base / "root", base / "bench"
        bench.mkdir()
        question = {"id": "fx-01", "repo": "o/fx", "url": str(self.repo_dir), "sha": self.sha, "prompt": "q",
                    "kind": "lookup", "oracle": [["src/app.py", 5, 14]], "oracle_lines": 10}
        (bench / "questions.jsonl").write_text(json.dumps(question) + "\n")
        binary = base / "fakepixel"
        binary.write_text(FAKE_HOME_BIN)
        binary.chmod(0o755)
        common = ["--root", str(root), "--questions", str(bench / "questions.jsonl"), "--out", str(base / "runs")]
        with mock.patch.dict(os.environ, {"HOME": str(installed)}):
            self.assertEqual(main(["--bin", str(binary), "--label", "installed"] + common), 0)
            self.assertEqual(main(["pinned", "--base", str(binary), "--head", str(binary), "--repo", str(repo),
                                   "--probe-commit", first, "--label", "same"] + common), 0)
        runs = base / "runs"
        # Every activation stops the clone's daemon before it indexes: a leftover daemon never serves a run.
        calls = [c for c in Path(str(binary) + ".calls").read_text().splitlines() if c in ("daemon stop", "prepare-repo .")]
        indexed = [i for i, c in enumerate(calls) if c == "prepare-repo ."]
        self.assertEqual(len(indexed), 3)                                    # three runs, one clone each
        self.assertNotIn(0, indexed)                                         # even the first run stops one first
        self.assertEqual([calls[i - 1] for i in indexed], ["daemon stop"] * 3)
        reports = {n: json.loads((runs / n / "report.json").read_text()) for n in ("installed", "same-base", "same-head")}
        # Only the frozen home has the fx probe, so only the pinned runs see its region.
        self.assertEqual(reports["installed"]["sessions"][0]["metrics"]["line_cov"], 0.0)
        for n in ("same-base", "same-head"):
            self.assertEqual(reports[n]["route"], "hook")
            # The hook route sends a brief-only payload with dedupe off and reads the meta from the decision row.
            self.assertEqual(reports[n]["sessions"][0]["brief_meta"],
                             {"gate": "open", "deadline_ms": 3000, "elapsed_ms": 42, "kind": "True-0"})
            self.assertEqual(reports[n]["sessions"][0]["metrics"]["line_cov"], 1.0)
            ident = reports[n]["probe_home"]
            self.assertEqual((ident["frozen"], ident["commit"]), (True, first))
            self.assertEqual(set(ident["probe_files"]), {"fx/pixel-plugin.toml", "fx/probe"})
            self.assertIn(f"frozen home `{ident['home']}` at commit `{first[:12]}`", (runs / n / "report.md").read_text())
        self.assertEqual(reports["installed"]["probe_home"]["frozen"], False)
        self.assertEqual(set(reports["installed"]["probe_home"]["probe_files"]), {"other/probe"})
        self.assertIn("**installed home**", (runs / "installed" / "report.md").read_text())
        sha = hashlib.sha256((bench / "questions.jsonl").read_bytes()).hexdigest()
        self.assertEqual({r["questions"]["sha256"] for r in reports.values()}, {sha})
        self.assertNotIn("Not like for like", (runs / "same-compare.md").read_text())
        mixed = render_compare([reports["installed"], reports["same-head"]], ["installed", "head"])
        self.assertIn("**installed** ran the installed probes", mixed)
        self.assertIn("Same binary, different probe files", mixed)
        other_bin = dict(reports["installed"], binary=dict(reports["installed"]["binary"], sha256="f" * 64))
        self.assertIn("both the binary and the probe files differ",
                      render_compare([other_bin, reports["same-head"]], ["a", "b"]))
        # A probe A/B: one binary, the probes of two commits, each side frozen and named.
        with mock.patch.dict(os.environ, {"HOME": str(installed)}):
            self.assertEqual(main(["pinned", "--base", str(binary), "--repo", str(repo), "--probe-commit", first,
                                   "--head-probe-commit", second, "--label", "ab"] + common), 0)
        ab = {n: json.loads((runs / f"ab-{n}" / "report.json").read_text())["probe_home"] for n in ("base", "head")}
        self.assertEqual((ab["base"]["commit"], ab["head"]["commit"]), (first, second))
        self.assertIn("Same binary, different probe files", (runs / "ab-compare.md").read_text())
        self.assertIn("different routes", render_compare([dict(reports["same-base"], route="command"),
                                                           reports["same-head"]], ["a", "b"]))
        self.assertIn("different routes", render_compare([{k: v for k, v in reports["same-base"].items() if k != "route"},
                                                           reports["same-head"]], ["old", "b"]))
        other = dict(reports["same-head"], questions={"sha256": "0" * 64})
        self.assertIn("different question files", render_compare([reports["same-base"], other], ["a", "b"]))

    def test_two_sources_at_once_are_refused(self):
        with self.assertRaises(SystemExit):
            main(["--bin", "x", "--questions", "q.jsonl", "--census-db", "c.db"])


def add_scoring_arguments(parser):
    parser.add_argument("--sessions", choices=("answer", "all"), default="answer", help="census only")
    qs.add_arguments(parser)
    parser.add_argument("--db", dest="census_db", default=None, help=argparse.SUPPRESS)  # the old spelling
    parser.add_argument("--out", default=None, help="output directory (default <root>/score/runs)")
    parser.add_argument("--only", default=None, help="comma-separated session ids")
    parser.add_argument("--cold", action="store_true",
                        help="skip the search-meaning warm-up: score the first prompt of a fresh index")
    parser.add_argument("--keep-daemons", action="store_true")
    parser.add_argument("--timeout", type=float, default=PROCESS_TIMEOUT_S,
                        help="maximum seconds per brief process (default: 300)")
    parser.add_argument("--route", choices=("hook", "command"), default="hook",
                        help="hook: the prompt-submit hook's brief, the live budget (default); command: `pixel brief`")


def pinned(args):
    """Score two binaries against one frozen probe home and the same questions, then compare them; or, with
    ``--head-probe-commit`` and one binary, the probes of two commits."""
    source = qs.open_source(args)
    root = Path(args.root)
    out = Path(args.out or root / "score" / "runs")

    def home_of(rev):
        if args.probe_home and rev is None:
            return Path(args.probe_home), verify_probe_home(args.probe_home)
        commit = sh(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"], cwd=args.repo).stdout.strip()
        home = out / f"probe-home-{commit[:12]}"
        return home, freeze_probe_home(args.repo, commit, home)

    homes = {"base": home_of(args.probe_commit), "head": home_of(args.head_probe_commit or args.probe_commit)}
    reports = []
    for side, binary in (("base", args.base), ("head", args.head or args.base)):
        home, manifest = homes[side]
        print(f"{side}: {binary} with the probes of {manifest['commit'][:12]} ({home})", flush=True)
        run = argparse.Namespace(**{**vars(args), "bin": binary, "label": f"{args.label}-{side}",
                                    "probe_home": str(home)})
        reports.append(json.loads(Path(score_all(run, root, source)).read_text(encoding="utf-8")))
    text = render_compare(reports, [f"{args.label}-base", f"{args.label}-head"])
    (out / f"{args.label}-compare.md").write_text(text, encoding="utf-8")
    print(text)
    print(f"wrote {out / (args.label + '-compare.md')}")
    return max(rep.get("result", {}).get("exit_code", 0) for rep in reports)


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if "--self-test" in argv:
        argv.remove("--self-test")
        result = unittest.main(argv=[sys.argv[0]] + argv, exit=False).result
        return 0 if result.wasSuccessful() else 1
    if argv[:1] == ["compare"]:
        parser = argparse.ArgumentParser(prog="score_brief.py compare")
        parser.add_argument("reports", nargs="+")
        parser.add_argument("--labels", default=None, help="comma-separated column names")
        args = parser.parse_args(argv[1:])
        reports = [json.loads(Path(p).read_text(encoding="utf-8")) for p in args.reports]
        print(render_compare(reports, args.labels.split(",") if args.labels else None))
        return max(rep.get("result", {}).get("exit_code", 0) for rep in reports)
    if argv[:1] == ["freeze-home"]:
        parser = argparse.ArgumentParser(prog="score_brief.py freeze-home",
                                         description="write the probes a binary built at --commit embeds")
        parser.add_argument("--commit", required=True, help="the pixel commit the scored binary was built from")
        parser.add_argument("--out", required=True, help="the home directory to write (empty or absent)")
        parser.add_argument("--repo", default=str(qs.REPO), help="the pixel checkout holding that commit")
        args = parser.parse_args(argv[1:])
        manifest = freeze_probe_home(args.repo, args.commit, args.out)
        print(f"{args.out}: {len(manifest['probes'])} probes of {manifest['commit'][:12]}")
        return 0
    if argv[:1] == ["pinned"]:
        parser = argparse.ArgumentParser(prog="score_brief.py pinned", description=pinned.__doc__)
        parser.add_argument("--base", required=True, help="the reference pixel binary")
        parser.add_argument("--head", default=None, help="the candidate pixel binary (default: --base)")
        home = parser.add_mutually_exclusive_group(required=True)
        home.add_argument("--probe-commit", help="freeze the probes of this pixel commit under <out>")
        home.add_argument("--probe-home", help="an existing frozen home (score_brief.py freeze-home)")
        parser.add_argument("--head-probe-commit", default=None,
                            help="the head side's probes instead (a probe A/B: same binary, two commits)")
        parser.add_argument("--repo", default=str(qs.REPO), help="the pixel checkout holding --probe-commit")
        parser.add_argument("--label", default="pinned", help="run name prefix (<label>-base, <label>-head)")
        add_scoring_arguments(parser)
        return pinned(parser.parse_args(argv[1:]))
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--bin", required=True, help="the pixel binary to score")
    parser.add_argument("--label", default=None, help="run name (default: the binary's file name)")
    parser.add_argument("--probe-home", default=None,
                        help="run the briefs with HOME set to this frozen home (score_brief.py freeze-home)")
    add_scoring_arguments(parser)
    args = parser.parse_args(argv)
    source = qs.open_source(args)
    report = json.loads(score_all(args, Path(args.root), source).read_text(encoding="utf-8"))
    return report["result"]["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
