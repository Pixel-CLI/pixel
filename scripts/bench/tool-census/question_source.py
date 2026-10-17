# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Where the zero-call test tools get their questions (issue #906).

The public brief bench (``eval/brief-bench/``: 56 answer-only questions on 7 pinned public repositories, with the
oracle lines of each and the plain-Codex baselines) is the default source of every tool that replays, scores or
runs a question: ``brief-check.py``, ``brief-zero-calls.py``, ``probe-test.py``, ``probe-translate.py``,
``probe-loop.py``, ``probe-equivalence.py``, ``brief-bench.py`` and ``score_brief.py``. The private census (20
prompts of #905, their ``census.db`` and the recorded Claude Code runs) is an optional second source, opted into
with ``--census-db <path>``; its prompts are never committed.

Both sources answer the same few questions, so a tool is written once:

    ids()                        the question ids
    sessions(only=None)          one dict per question: ``{id, repo, label, url, sha, prompt, kind, oracle}`` where
                                 ``repo`` names the pinned clone under ``<root>/cache/`` and ``oracle`` is the list
                                 of ``[path, lo, hi]`` lines the question needed (``None`` for the census, whose
                                 oracle is rebuilt from its recorded calls)
    prepare(root, only=None)     the pinned clones under ``<root>/cache/``, idempotent: ``{repo: sha}``
    baseline_trace(id)           ``(label, calls)`` of the recorded no-brief session, or ``None``; a bench call is
                                 ``{tool, reads: [[path, lo, hi]], commits: [sha7]}`` (aggregates only)
    reference_calls()            ``{id: n}`` calls the no-brief session made, the column a live run is compared with

``--root`` is where every tool keeps its state (``cache/``, ``ws/``, ``work/``, ``score/``, ``zero-calls/``); it
defaults to ``$PIXEL_CENSUS_ROOT`` or ``$TMPDIR/pixel-tool-census``, and to the directory of ``--census-db`` when
that is given. Standard library only; run with ``python3 -I``.
"""

import fcntl
import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
BENCH_DIR = REPO / "eval" / "brief-bench"
DEFAULT_QUESTIONS = BENCH_DIR / "questions.jsonl"
CENSUS_DIRS = tuple(Path(p) for p in (os.environ.get("PIXEL_CENSUS_DIR"), HERE) if p)


def default_root():
    """The state root: ``$PIXEL_CENSUS_ROOT``, else ``pixel-tool-census`` under the temporary directory."""
    return Path(os.environ.get("PIXEL_CENSUS_ROOT") or Path(tempfile.gettempdir()) / "pixel-tool-census")


def load_module(name, directory=None):
    """The census module ``<name>.py`` from ``directory``, else ``$PIXEL_CENSUS_DIR``, else this directory."""
    for candidate in ([Path(directory)] if directory else list(CENSUS_DIRS)):
        path = candidate / f"{name}.py"
        if path.exists():
            spec = importlib.util.spec_from_file_location(name, path)
            module = importlib.util.module_from_spec(spec)
            sys.modules[name] = module
            spec.loader.exec_module(module)
            return module
    raise SystemExit(f"{name}.py not found: pass --census-dir or set PIXEL_CENSUS_DIR")


def copy_tree(src, dest):
    """An independent copy of a clone: APFS copy-on-write where ``cp -c`` exists (no disk cost), else a plain copy."""
    proc = subprocess.run(["cp", "-c", "-R", str(src), str(dest)], capture_output=True)
    if proc.returncode != 0:
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["cp", "-R", str(src), str(dest)], check=True, capture_output=True)


def cache_name(repo):
    """The directory under ``<root>/cache/`` of a ``owner/name`` repository."""
    return repo.replace("/", "__")


def read_jsonl(path):
    path = Path(path)
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []


def git(args, cwd=None, check=True):
    proc = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise SystemExit(f"git {' '.join(args)}: {proc.stderr.strip()[-300:]}")
    return proc


# --------------------------------------------------------------------------- the public bench

class BenchSource:
    """``questions.jsonl`` and the sibling ``baselines.jsonl`` of a bench directory."""
    kind = "bench"
    label = "bench"
    reference_label = "bench baseline calls"

    def __init__(self, questions=DEFAULT_QUESTIONS):
        self.path = Path(questions)
        if not self.path.exists():
            raise SystemExit(f"{self.path}: no such question file")
        self.rows = read_jsonl(self.path)
        self.baselines = {b["id"]: b for b in read_jsonl(self.path.with_name("baselines.jsonl"))}

    def ids(self):
        return [r["id"] for r in self.rows]

    def sessions(self, only=None):
        return [{"id": r["id"], "repo": cache_name(r["repo"]), "label": r["repo"], "url": r["url"], "sha": r["sha"],
                 "prompt": r["prompt"], "kind": r.get("kind"), "oracle": r.get("oracle") or [],
                 "oracle_commits": r.get("oracle_commits") or []}
                for r in self.rows if not only or r["id"] in only]

    def get(self, qid):
        for session in self.sessions({qid}):
            return session
        raise SystemExit(f"no question {qid} in {self.path}")

    def baseline_trace(self, qid):
        base = self.baselines.get(qid)
        if not base or not base.get("calls"):
            return None
        return f"bench baseline ({base.get('runs', 1)} run)", base["calls"]

    def reference_calls(self):
        return {qid: sorted(b["n_calls"])[len(b["n_calls"]) // 2] for qid, b in self.baselines.items()
                if b.get("n_calls")}

    def prepare(self, root, only=None):
        """Clone each pinned repository once into ``<root>/cache/<owner__name>`` and check out its commit."""
        cache = Path(root) / "cache"
        cache.mkdir(parents=True, exist_ok=True)
        pins = {}
        with open(cache / ".prepare.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for s in self.sessions(only):
                dest = cache / s["repo"]
                if s["repo"] in pins:
                    continue
                if not (dest / ".git").exists():
                    git(["clone", "-q", s["url"], str(dest)])
                if git(["rev-parse", "HEAD"], dest).stdout.strip() != s["sha"]:
                    git(["checkout", "-q", "--detach", s["sha"]], dest)
                if git(["status", "--porcelain"], dest).stdout.strip():
                    raise SystemExit(f"{s['repo']}: the cache clone is dirty")
                pins[s["repo"]] = s["sha"]
        return pins


# --------------------------------------------------------------------------- the private census

class CensusSource:
    """The answer-only sessions of a local ``census.db`` (#905). The prompts stay in that database."""
    kind = "census"
    label = "census"
    reference_label = "census Claude calls"

    def __init__(self, db_path, census_dir=None):
        self.db_path = Path(db_path)
        self.census_dir = census_dir
        if not self.db_path.exists():
            raise SystemExit(f"{self.db_path}: no census database (the census prompts are private and local; "
                             "drop --census-db to use the public bench)")

    def _rows(self, only=None):
        db = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True)
        rows = db.execute("SELECT id, repo, sha, prompt, category FROM sessions WHERE source='census' "
                          "AND mode='answer' AND ok=1 ORDER BY id").fetchall()
        db.close()
        return [r for r in rows if not only or r[0] in only]

    def ids(self):
        return [r[0] for r in self._rows()]

    def sessions(self, only=None):
        return [{"id": i, "repo": repo, "label": repo, "url": None, "sha": sha, "prompt": prompt, "kind": category,
                 "oracle": None} for i, repo, sha, prompt, category in self._rows(only)]

    def get(self, qid):
        for session in self.sessions({qid}):
            return session
        raise SystemExit(f"no answer-only census question {qid}")

    def baseline_trace(self, qid):
        return None

    def reference_calls(self):
        db = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True)
        rows = db.execute("SELECT session, COUNT(*) FROM calls GROUP BY session").fetchall()
        db.close()
        return dict(rows)

    def prepare(self, root, only=None):
        return load_module("run", self.census_dir).prepare_cache(Path(root))


# --------------------------------------------------------------------------- command line

def add_arguments(parser):
    """``--questions``, ``--census-db`` and ``--root``, the same on every tool."""
    parser.add_argument("--questions", default=None, metavar="JSONL",
                        help=f"question file, with baselines.jsonl beside it (default: {DEFAULT_QUESTIONS.relative_to(REPO)})")
    parser.add_argument("--census-db", default=None, metavar="DB",
                        help="the private census instead of the public bench: its census.db (local only)")
    parser.add_argument("--root", default=None,
                        help="state directory (default: $PIXEL_CENSUS_ROOT, else the directory of --census-db, "
                             "else $TMPDIR/pixel-tool-census)")


def resolve_root(args):
    if getattr(args, "root", None):
        return Path(args.root)
    if getattr(args, "census_db", None):
        return Path(args.census_db).resolve().parent
    return default_root()


def open_source(args):
    """The source the arguments name; ``args.root`` is filled in. Both options at once is an error."""
    questions, census_db = getattr(args, "questions", None), getattr(args, "census_db", None)
    if questions and census_db:
        raise SystemExit("--questions and --census-db name two sources: pick one")
    args.root = str(resolve_root(args))
    if census_db:
        return CensusSource(census_db, getattr(args, "census_dir", None))
    return BenchSource(questions or DEFAULT_QUESTIONS)


# --------------------------------------------------------------------------- self-test

def fixture_bench(base):
    """A one-question bench on a throwaway local git repository: ``(questions path, sha)``."""
    repo = Path(base) / "origin"
    (repo / "src").mkdir(parents=True)
    (repo / "src" / "lib.py").write_text("".join(f"line_{i} = {i}\n" for i in range(1, 21)))
    env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
           "GIT_COMMITTER_EMAIL": "t@t", "GIT_CONFIG_GLOBAL": "/dev/null"}
    for argv in (["init", "-q", "-b", "main"], ["add", "."], ["commit", "-q", "-m", "init"]):
        subprocess.run(["git", *argv], cwd=repo, env=env, check=True, capture_output=True)
    sha = git(["rev-parse", "HEAD"], repo).stdout.strip()
    bench = Path(base) / "bench"
    bench.mkdir()
    question = {"id": "t-01", "repo": "o/t", "url": str(repo), "sha": sha, "prompt": "where is line_7 defined",
                "kind": "lookup", "oracle": [["src/lib.py", 5, 9]], "oracle_lines": 5}
    (bench / "questions.jsonl").write_text(json.dumps(question) + "\n")
    (bench / "baselines.jsonl").write_text(json.dumps(
        {"id": "t-01", "runs": 1, "n_calls": [2], "calls": [{"tool": "sh", "reads": [["src/lib.py", 5, 9]], "commits": []},
                                                              {"tool": "sh", "reads": [], "commits": []}]}) + "\n")
    return bench / "questions.jsonl", sha


class SelfTest(unittest.TestCase):
    def test_the_shipped_bench_is_the_default_source(self):
        source = BenchSource()
        self.assertEqual(source.path, BENCH_DIR / "questions.jsonl")
        self.assertGreaterEqual(len(source.ids()), 50)
        first = source.sessions()[0]
        self.assertEqual(first["repo"], cache_name(first["label"]))
        self.assertNotIn("/", first["repo"])
        self.assertTrue(first["oracle"])

    def test_the_source_arguments_default_to_the_bench_and_refuse_two_sources(self):
        import argparse
        parser = argparse.ArgumentParser()
        add_arguments(parser)
        args = parser.parse_args([])
        source = open_source(args)
        self.assertIsInstance(source, BenchSource)
        self.assertEqual(Path(args.root), default_root())
        with self.assertRaises(SystemExit):
            open_source(parser.parse_args(["--questions", "q.jsonl", "--census-db", "c.db"]))

    def test_a_census_db_is_optional_and_a_missing_one_is_explained(self):
        with self.assertRaises(SystemExit) as caught:
            CensusSource("/nonexistent/census.db")
        self.assertIn("public bench", str(caught.exception))

    def test_the_census_db_directory_is_the_root_unless_one_is_given(self):
        import argparse
        parser = argparse.ArgumentParser()
        add_arguments(parser)
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "census.db"
            sqlite3.connect(db).close()
            args = parser.parse_args(["--census-db", str(db)])
            self.assertIsInstance(open_source(args), CensusSource)
            self.assertEqual(Path(args.root), Path(tmp).resolve())
            args = parser.parse_args(["--census-db", str(db), "--root", "/x"])
            open_source(args)
            self.assertEqual(args.root, "/x")

    def test_a_census_reads_answer_sessions_and_their_call_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "census.db"
            db = sqlite3.connect(db_path)
            db.execute("CREATE TABLE sessions(id TEXT, source TEXT, repo TEXT, sha TEXT, category TEXT, mode TEXT, "
                       "prompt TEXT, ok INTEGER)")
            db.execute("CREATE TABLE calls(session TEXT, seq INTEGER)")
            db.executemany("INSERT INTO sessions VALUES (?,?,?,?,?,?,?,?)", [
                ("a-1", "census", "a", "s1", "lookup", "answer", "q1", 1), ("a-2", "census", "a", "s2", "x", "edit", "q2", 1),
                ("a-3", "census", "a", "s3", "x", "answer", "q3", 0), ("a-4", "history", "a", "s4", "x", "answer", "q4", 1),
                ("a-5", "census", "b", "s5", "x", "answer", "q5", 1)])
            db.executemany("INSERT INTO calls VALUES (?,?)", [("a-1", 1), ("a-1", 2), ("a-5", 1)])
            db.commit()
            db.close()
            source = CensusSource(db_path)
            self.assertEqual(source.ids(), ["a-1", "a-5"])
            self.assertEqual([s["prompt"] for s in source.sessions({"a-5"})], ["q5"])
            self.assertEqual(source.reference_calls(), {"a-1": 2, "a-5": 1})
            self.assertIsNone(source.baseline_trace("a-1"))
            with self.assertRaises(SystemExit):
                source.get("a-2")

    def test_prepare_clones_each_pinned_repository_once_at_its_commit(self):
        with tempfile.TemporaryDirectory() as tmp:
            questions, sha = fixture_bench(tmp)
            source = BenchSource(questions)
            root = Path(tmp) / "root"
            self.assertEqual(source.prepare(root), {"o__t": sha})
            clone = root / "cache" / "o__t"
            self.assertEqual(git(["rev-parse", "HEAD"], clone).stdout.strip(), sha)
            (clone / ".git" / "marker").write_text("kept")
            self.assertEqual(source.prepare(root), {"o__t": sha})   # idempotent, no second clone
            self.assertEqual((clone / ".git" / "marker").read_text(), "kept")

    def test_baseline_trace_and_reference_calls_come_from_baselines_jsonl(self):
        with tempfile.TemporaryDirectory() as tmp:
            questions, _sha = fixture_bench(tmp)
            source = BenchSource(questions)
            label, calls = source.baseline_trace("t-01")
            self.assertIn("bench baseline", label)
            self.assertEqual(calls[0]["reads"], [["src/lib.py", 5, 9]])
            self.assertEqual(source.reference_calls(), {"t-01": 2})
            self.assertIsNone(source.baseline_trace("nope"))

    def test_copy_tree_makes_an_independent_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            src, dest = Path(tmp) / "src", Path(tmp) / "dest"
            src.mkdir()
            (src / "a").write_text("1")
            copy_tree(src, dest)
            (dest / "a").write_text("2")
            self.assertEqual((src / "a").read_text(), "1")

    def test_no_tool_hardcodes_a_machine_or_a_private_scratch_path(self):
        """Every script of the zero-call loop runs on a clean checkout: no home directory, no scratch of one account."""
        names = ("brief-check", "brief-zero-calls", "brief-bench", "probe-test", "probe-translate", "probe-loop",
                 "probe-chain", "probe-equivalence", "extract-probes")
        paths = [REPO / "scripts" / "bench" / f"{n}.py" for n in names] + [p for p in sorted(HERE.glob("*.py"))
                                                                  if p.name != Path(__file__).name]
        needles = ("/Users/" + "livio", "claude" + "-501")
        bad = [f"{p.name}: {n}" for p in paths for n in needles if n in p.read_text(encoding="utf-8")]
        self.assertEqual(bad, [])

    def test_the_private_census_prompts_are_git_ignored(self):
        ignored = (REPO / ".gitignore").read_text().splitlines()
        self.assertIn("scripts/bench/tool-census/manifest.json", ignored)
        self.assertIn("census.db*", ignored)


if __name__ == "__main__":
    sys.exit(0 if unittest.main(argv=[sys.argv[0]], exit=False).result.wasSuccessful() else 1)
