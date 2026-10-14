#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Graph-sensitive bench: judged Rust edges, held or not by each Rust source.

The set (`eval/graph-edges/rust-85bede7d.tsv`) is 160 call and reference
edges of this repository at `85bede7d`, each judged `correct` or `wrong`
against the source (README beside it). For each Rust source of the graph
(`PIXEL_GRAPH_SOURCE_RUST=v1`, this graph's own resolver, and `mvp`,
pixel-mvp's index), the bench builds the graph of a checkout at that commit
and counts which judged edges it holds: an edge is held when the graph has
an edge of that kind from the caller to the callee, both named by path and
qualified name, at a site line within 3 of the judged one.

    python3 scripts/bench-graph-edges.py --pixel target/release/pixel --repo <checkout at 85bede7d>
    python3 scripts/bench-graph-edges.py --self-test

Exit 1 when the `mvp` source holds a judged-wrong edge: the bar the switch
was taken on. `--json` prints the counts instead of the table.
"""
import argparse
import csv
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SET = ROOT / "eval/graph-edges/rust-85bede7d.tsv"
COMMIT = "85bede7d"
TOLERANCE = 3
SOURCES = ("v1", "mvp")
KIND = {"call": "calls", "reference": "references"}


def load_set(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f, delimiter="\t"))


def build(pixel, repo, source):
    """Build the graph of `repo` with the Rust source `source`, isolated
    from the user's home and daemon; return the graph's path."""
    state = Path(repo) / ".pixel"
    subprocess.run(["rm", "-rf", str(state)], check=True)
    home = tempfile.mkdtemp(prefix="bench-graph-edges-")
    env = dict(
        os.environ,
        HOME=home,
        XDG_CACHE_HOME=f"{home}/cache",
        XDG_STATE_HOME=f"{home}/state",
        XDG_DATA_HOME=f"{home}/data",
        XDG_CONFIG_HOME=f"{home}/config",
        PIXEL_DAEMON_AUTO_START="0",
        PIXEL_METRICS="0",
        PIXEL_NO_UPDATE_CHECK="1",
        PIXEL_BRIEF_LOG="0",
        PIXEL_GRAPH_SOURCE_RUST=source,
    )
    subprocess.run(
        [pixel, "rebuild-graph", str(repo), "--json"],
        env=env,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return state / "graph.v2.db"


def held(db, rows):
    """Per judged row: `None` when the graph lacks the caller or the callee
    symbol, else whether it holds the edge."""
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    symbols = {}
    for sid, path, qualified in con.execute(
        "SELECT s.id, f.path, s.qualified FROM symbols s JOIN files f ON f.id = s.file_id"
    ):
        symbols.setdefault((path, qualified), []).append(sid)
    out = []
    for row in rows:
        src = symbols.get((row["src_path"], row["src"]), [])
        dst = symbols.get((row["dst_path"], row["dst"]), [])
        if not src or not dst:
            out.append(None)
            continue
        line = int(row["line"])
        marks = ",".join("?" * len(src))
        dmarks = ",".join("?" * len(dst))
        found = con.execute(
            f"SELECT count(*) FROM edges WHERE kind = ? AND src_id IN ({marks})"
            f" AND dst_id IN ({dmarks}) AND site_line BETWEEN ? AND ?",
            [KIND[row["kind"]], *src, *dst, line - TOLERANCE, line + TOLERANCE],
        ).fetchone()[0]
        out.append(found > 0)
    con.close()
    return out


def score(rows, marks):
    """Counts of one source: judged edges whose two ends the graph has
    (`representable`), and of those, the ones it holds, by verdict."""
    counts = {
        verdict: {"representable": 0, "held": 0} for verdict in ("correct", "wrong")
    }
    for row, mark in zip(rows, marks):
        if mark is None:
            continue
        bucket = counts[row["verdict"]]
        bucket["representable"] += 1
        bucket["held"] += int(mark)
    return counts


def head_of(repo):
    return subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--pixel")
    ap.add_argument("--repo")
    ap.add_argument("--set", default=str(SET))
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args(argv)
    if a.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        return 0 if unittest.TextTestRunner(verbosity=1).run(suite).wasSuccessful() else 1
    if not a.pixel or not a.repo:
        ap.error("--pixel and --repo are required")
    if not head_of(a.repo).startswith(COMMIT):
        ap.error(f"{a.repo} is not at {COMMIT}, the commit the set was judged on")
    rows = load_set(a.set)
    report = {}
    for source in SOURCES:
        report[source] = score(rows, held(build(a.pixel, a.repo, source), rows))
    if a.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"{len(rows)} judged Rust edges at {COMMIT} (a site within {TOLERANCE} lines is the same edge)")
        print("source  correct held / with both ends   wrong held / with both ends")
        for source, c in report.items():
            ok, bad = c["correct"], c["wrong"]
            print(
                f"{source:<7} {ok['held']:>4} / {ok['representable']:<20} {bad['held']:>4} / {bad['representable']}"
            )
    return 1 if report["mvp"]["wrong"]["held"] else 0


class SelfTest(unittest.TestCase):
    def graph(self, edges):
        path = Path(tempfile.mkdtemp()) / "graph.v2.db"
        con = sqlite3.connect(path)
        con.executescript(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);"
            "CREATE TABLE symbols (id INTEGER PRIMARY KEY, file_id INTEGER, qualified TEXT);"
            "CREATE TABLE edges (src_id INTEGER, dst_id INTEGER, kind TEXT, site_line INTEGER);"
            "INSERT INTO files VALUES (1, 'a.rs'), (2, 'b.rs');"
            "INSERT INTO symbols VALUES (10, 1, 'run'), (20, 2, 'join'), (21, 2, 'S::len');"
        )
        con.executemany("INSERT INTO edges VALUES (?, ?, ?, ?)", edges)
        con.commit()
        con.close()
        return path

    def row(self, dst, line, kind="call", verdict="wrong"):
        return {
            "src_path": "a.rs",
            "src": "run",
            "dst_path": "b.rs",
            "dst": dst,
            "line": str(line),
            "kind": kind,
            "verdict": verdict,
        }

    def test_an_edge_is_held_within_the_line_tolerance_and_of_its_kind(self):
        db = self.graph([(10, 20, "calls", 12)])
        rows = [
            self.row("join", 9),
            self.row("join", 15),
            self.row("join", 16),
            self.row("join", 12, kind="reference"),
        ]
        self.assertEqual(held(db, rows), [True, True, False, False])

    def test_a_missing_symbol_is_not_representable_and_not_scored(self):
        db = self.graph([(10, 21, "calls", 5)])
        rows = [self.row("S::len", 5, verdict="correct"), self.row("gone", 5)]
        marks = held(db, rows)
        self.assertEqual(marks, [True, None])
        self.assertEqual(
            score(rows, marks),
            {
                "correct": {"representable": 1, "held": 1},
                "wrong": {"representable": 0, "held": 0},
            },
        )

    def test_the_set_is_well_formed(self):
        rows = load_set(SET)
        self.assertEqual(len(rows), 160)
        self.assertEqual({r["kind"] for r in rows}, {"call", "reference"})
        self.assertEqual({r["verdict"] for r in rows}, {"correct", "wrong"})
        self.assertEqual(len({r["id"] for r in rows}), len(rows))
        for r in rows:
            self.assertTrue(int(r["line"]) > 0, r)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
