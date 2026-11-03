#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""The Ruby hybrid's correctness on one corpus, against v1's own Ruby graph.

usage: python3 -I compare.py V1_DB HYBRID_DB MVP_DB REPO OUT_DIR
           [--known known.tsv] [--judgments DIR] [--n-added 80] [--n-raised 40]

V1_DB and HYBRID_DB are `.pixel/graph.v2.db` built by the same binary with
PIXEL_GRAPH_SOURCE_RUBY unset and `hybrid` (build.sh); MVP_DB is the hybrid
clone's `.pixel/.pixel-mvp/graph.db`. Standard library only.

Writes to OUT_DIR:
- summary.json: every count printed below;
- delta-<class>.tsv: the edges of one graph only, keyed (caller, callee,
  site line) as pixel-research-mvp's hybridcmp.py keys them;
- sample-v1-only.tsv, sample-hybrid-only.tsv, sample-raised.tsv: seeded
  samples (all rows when a side has no more than its N) of the call edges
  the merge dropped, added, and raised to `exact`, each row carrying the
  verdict known.tsv already holds for the same edge (caller path, line,
  callee path, callee, callee line).

Then prints one line per category and per case of pixel-mvp#111, and a
verdict. With --judgments, the verdicts of judgments/<sample>*.tsv
(id<TAB>verdict<TAB>reason) complete the samples' reused ones, and the
estimate of correct call edges is printed (v1's level from
v1-sample.tsv + judgments/v1-*.tsv, pixel-research-mvp 2026-10-15).

Exit status: 0 when every case holds and no judged edge the merge wrote is
wrong; 1 otherwise (an open case, a wrong target, a missing verdict).
"""
import argparse
import collections
import csv
import glob
import json
import math
import os
import random
import re
import sqlite3
import sys

csv.field_size_limit(10**9)

SEED = "20261018-correctness"
TEST_PATH = re.compile(r"(^|/)(spec|test)/|_spec\.rb$|_test\.rb$")
COLS = ["id", "caller_path", "line", "site_text", "caller", "callee", "callee_path",
        "callee_line", "written", "tier"]


def klass(kind, receiver):
    """call-self (no receiver), call, route, association, reference."""
    r = receiver or ""
    if kind == "calls":
        return "call-self" if not r or r == "self" else "call"
    for name in ("route", "association"):
        if r.startswith(f":{name} "):
            return name
    return "reference"


def connect(db):
    return sqlite3.connect(f"file:{db}?mode=ro", uri=True)


def load_edges(db):
    """class -> key -> row, the Ruby `calls` and `references` edges."""
    q = """SELECT s.uid, s.qualified, fs.path, d.uid, d.qualified, fd.path, d.start_line,
                  e.site_line, e.kind, e.tier, e.receiver, e.callee
             FROM edges e JOIN symbols s ON s.id = e.src_id JOIN files fs ON fs.id = s.file_id
             JOIN symbols d ON d.id = e.dst_id JOIN files fd ON fd.id = d.file_id
            WHERE fs.lang = 'ruby' AND e.kind IN ('calls', 'references')"""
    out = collections.defaultdict(dict)
    for (su, sq, sp, du, dq, dp, dl, line, kind, tier, recv, callee) in connect(db).execute(q):
        row = out[klass(kind, recv)].setdefault((su, du, line), dict(
            caller=sq, caller_path=sp, line=line, callee=dq, callee_path=dp, callee_line=dl,
            written=((recv or "") + " " + (callee or "")).strip(), name=callee or "",
            tiers=set()))
        row["tiers"].add(tier)
    return out


def load_unresolved(db):
    """(path, line, name) of every Ruby `calls` site left unresolved."""
    q = """SELECT f.path, u.site_line, u.name FROM unresolved_calls u
             JOIN files f ON f.id = u.file_id WHERE f.lang = 'ruby' AND u.kind = 'calls'"""
    return collections.Counter(connect(db).execute(q))


def sites(edges, unresolved):
    """(path, line, name) -> 'edge' or 'unresolved', for call sites."""
    out = {}
    for k in ("call", "call-self"):
        for r in edges.get(k, {}).values():
            out[(r["caller_path"], r["line"], r["name"])] = "edge"
    for key in unresolved:
        out.setdefault(key, "unresolved")
    return out


def src_line(repo, path, line, cache={}):
    if path not in cache:
        try:
            with open(os.path.join(repo, path), errors="replace") as f:
                cache[path] = f.read().split("\n")
        except OSError:
            cache[path] = []
    lines = cache[path]
    return lines[line - 1].strip()[:200] if 0 < line <= len(lines) else ""


def flat(r):
    return {c: str(r[c]).replace("\t", " ").replace("\n", " ") for c in r}


def identity(r):
    return (r["caller_path"], str(r["line"]), r["callee_path"], r["callee"], str(r["callee_line"]))


def write_sample(path, rows, known):
    with open(path, "w") as f:
        f.write("\t".join(COLS + ["reused_verdict", "from"]) + "\n")
        for r in rows:
            v, o = known.get(identity(r), ("", ""))
            f.write("\t".join([flat(r)[c] for c in COLS] + [v, o]) + "\n")


def seeded(rows, n, label):
    if len(rows) <= n:
        return rows
    return random.Random(f"{SEED}-{label}").sample(rows, n)


def wilson(k, n, z=1.96):
    if n == 0:
        return 0.0, 1.0
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return c - h, c + h


def verdicts(out_dir, judgments, sample):
    """id -> verdict for every row of OUT_DIR/<sample>.tsv, None when one lacks it."""
    rows = list(csv.DictReader(open(os.path.join(out_dir, f"{sample}.tsv")), delimiter="\t"))
    got = {r["id"]: r["reused_verdict"] for r in rows if r.get("reused_verdict")}
    for path in sorted(glob.glob(os.path.join(judgments, f"{sample}*.tsv"))):
        for line in open(path):
            p = line.rstrip("\n").split("\t")
            if len(p) >= 2 and p[1].strip().lower() in ("correct", "wrong", "undecidable"):
                got[p[0]] = p[1].strip().lower()
    missing = [r["id"] for r in rows if r["id"] not in got]
    return rows, got, missing


def mvp_cases(mvp_db, repo, edges_h, edges_v):
    """The cases pixel-mvp#111 listed, as (name, holds, evidence)."""
    con = connect(mvp_db)
    cases = []
    calls = con.execute("SELECT sum(resolved), count(*) FROM edges WHERE kind = 'call'").fetchone()
    cases.append(("pixel-mvp resolves more Ruby call sites than #111's 2 251",
                  calls[0] > 2251, f"{calls[0]} of {calls[1]} call sites resolved"))
    rv, rh = len(edges_v.get("route", {})), len(edges_h.get("route", {}))
    mvp_routes = con.execute("SELECT sum(resolved), count(*) FROM edges WHERE kind = 'route'").fetchone()
    cases.append(("routes: hybrid > 0 and >= v1 (#111: 0 against 702)", rh > 0 and rh >= rv,
                  f"hybrid {rh}, v1 {rv}; pixel-mvp resolves {mvp_routes[0]} of {mvp_routes[1]}"))
    # A call on self: the context's receiver field is empty.
    self_q = """SELECT e.resolved, e.dst, s.file FROM edges e JOIN symbols s ON s.id = e.src
                 WHERE e.kind = 'call' AND e.context LIKE e.target || char(31) || char(30) || '%'
                   AND s.file LIKE 'app/controllers/%'"""
    own = inherited = into_app = 0
    total = 0
    for resolved, dst, path in con.execute(self_q):
        total += 1
        if resolved:
            if dst.startswith(path + "#"):
                own += 1
            else:
                inherited += 1
                into_app += dst.startswith("app/controllers/application_controller.rb#")
    leaves = {r[0] for r in con.execute(
        "SELECT leaf FROM symbols WHERE file = 'app/controllers/application_controller.rb'")}
    abstained = collections.Counter()
    for (target,) in con.execute(
            """SELECT e.target FROM edges e JOIN symbols s ON s.id = e.src
                WHERE e.kind = 'call' AND e.resolved = 0 AND s.file LIKE 'app/controllers/%'
                  AND s.file != 'app/controllers/application_controller.rb'
                  AND e.context LIKE e.target || char(31) || char(30) || '%'"""):
        if target in leaves:
            abstained[target] += 1
    top = ", ".join(f"{n} {c}" for n, c in abstained.most_common(3))
    cases.append(("self-calls from a controller into ApplicationController resolve (#111: the "
                  "chain's first step is unknown)", into_app > 0,
                  f"{into_app} resolved into ApplicationController; {sum(abstained.values())} "
                  f"abstained on a name it defines ({top}); controllers' self-calls: {own} to "
                  f"their own class, {inherited} inherited, of {total}"))
    unnamed = []
    for path, facts in con.execute("SELECT path, facts FROM files WHERE facts LIKE '%tainted%'"):
        for record in facts.split("\x1e"):
            fields = record.split("\x1f")
            if fields[0] == "tainted" and (len(fields) < 2 or fields[1] == ""):
                unnamed.append(path)
                break
    app = [p for p in unnamed if not TEST_PATH.search(p)]
    cases.append(("no definition of any name on an object the rules cannot name in app code "
                  "(#111: unnamed definition on an unknown receiver)", not app,
                  f"{len(app)} non-test files ({', '.join(app[:3])}); {len(unnamed) - len(app)} "
                  "test files, which taint only test callers"))
    anon = []
    assigned = re.compile(r"^\s*(?:[A-Z]\w*(?:::[A-Z]\w*)*\s*\|\|?=|[A-Z]\w*(?:::[A-Z]\w*)*\s*=)")
    for path, in con.execute("SELECT path FROM files WHERE lang = 'ruby'"):
        if TEST_PATH.search(path):
            continue
        try:
            text = open(os.path.join(repo, path), errors="replace").read().split("\n")
        except OSError:
            continue
        for i, line in enumerate(text, 1):
            if "Class.new" in line and not assigned.match(line) and not line.lstrip().startswith("#"):
                anon.append(f"{path}:{i}")
    tainted_anon = [a for a in anon if a.split(":")[0] in unnamed]
    cases.append(("no anonymous Class.new in app code makes lookups abstain repository-wide "
                  "(#111: Class.new(P))", not tainted_anon,
                  f"{len(anon)} unassigned Class.new in non-test files ({', '.join(anon[:3])}), "
                  f"{len(tainted_anon)} of them in a file with an unnamed taint"))
    return cases


def main():
    ap = argparse.ArgumentParser()
    for a in ("v1", "hybrid", "mvp", "repo", "out"):
        ap.add_argument(a)
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--known", default=os.path.join(here, "known.tsv"))
    ap.add_argument("--judgments")
    ap.add_argument("--n-added", type=int, default=80)
    ap.add_argument("--n-raised", type=int, default=40)
    ap.add_argument("--n-dropped", type=int, default=80)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    ev, eh = load_edges(a.v1), load_edges(a.hybrid)
    uv, uh = load_unresolved(a.v1), load_unresolved(a.hybrid)
    known = {}
    if os.path.exists(a.known):
        for line in open(a.known):
            p = line.rstrip("\n").split("\t")
            if len(p) >= 7 and p[0] == "gumroad":
                known[tuple(p[1:6])] = (p[6], p[7] if len(p) > 7 else "")
    summary, failed = {}, []
    print(f"{'class':<12} {'v1':>8} {'hybrid':>8} {'both':>8} {'dropped':>8} {'added':>8} {'raised':>8}")
    deltas = {}
    for k in ("call", "call-self", "route", "association", "reference"):
        V, H = ev.get(k, {}), eh.get(k, {})
        both = V.keys() & H.keys()
        raised = [key for key in both if V[key]["tiers"] != H[key]["tiers"]]
        s = dict(v1=len(V), hybrid=len(H), both=len(both), v1_only=len(V.keys() - H.keys()),
                 hybrid_only=len(H.keys() - V.keys()), tier_changed=len(raised))
        summary[k] = s
        print(f"{k:<12} {s['v1']:>8} {s['hybrid']:>8} {s['both']:>8} {s['v1_only']:>8} "
              f"{s['hybrid_only']:>8} {s['tier_changed']:>8}")
        rows = {}
        for side, src, keys in (("v1-only", V, V.keys() - H.keys()), ("hybrid-only", H, H.keys() - V.keys()),
                                ("raised", H, raised)):
            ordered = sorted(keys, key=lambda x: (src[x]["caller_path"], src[x]["line"], src[x]["callee"],
                                                  src[x]["callee_path"], src[x]["callee_line"]))
            rows[side] = [dict(src[key], id=f"{k}-{side}-{i:05}", tier="/".join(sorted(src[key]["tiers"])),
                               site_text=src_line(a.repo, src[key]["caller_path"], src[key]["line"]))
                          for i, key in enumerate(ordered, 1)]
        deltas[k] = rows
        with open(os.path.join(a.out, f"delta-{k}.tsv"), "w") as f:
            f.write("\t".join(["side"] + COLS) + "\n")
            for side in ("v1-only", "hybrid-only"):
                for r in rows[side]:
                    f.write("\t".join([side] + [flat(r)[c] for c in COLS]) + "\n")
    # Calls (with and without a receiver) are sampled together, as v1's level was.
    calls = {side: deltas["call"][side] + deltas["call-self"][side] for side in ("v1-only", "hybrid-only", "raised")}
    for name, side, n in (("sample-v1-only", "v1-only", a.n_dropped), ("sample-hybrid-only", "hybrid-only", a.n_added),
                          ("sample-raised", "raised", a.n_raised)):
        write_sample(os.path.join(a.out, f"{name}.tsv"), seeded(calls[side], n, side), known)
    write_sample(os.path.join(a.out, "sample-routes-added.tsv"),
                 seeded(deltas["route"]["hybrid-only"], 40, "route"), known)
    # Verdicts pixel-research-mvp already gave to edges of today's deltas,
    # whatever sample they came from (its 2026-10-16 samples among them).
    carried = {}
    for side, rows in list(calls.items()) + [("route-added", deltas["route"]["hybrid-only"])]:
        c = collections.Counter(known[identity(r)][0] for r in rows if identity(r) in known)
        carried[side] = dict(rows=len(rows), **c)
        print(f"known verdicts on the whole {side} delta: {len(rows)} rows, "
              + ", ".join(f"{n} {v}" for v, n in sorted(c.items())))
    summary["known_on_delta"] = carried

    # Unresolved-site honesty, per call site (file, line, name written).
    sv, sh = sites(ev, uv), sites(eh, uh)
    moves = collections.Counter((sv.get(key, "absent"), sh.get(key, "absent")) for key in sv.keys() | sh.keys())
    summary["sites"] = {f"{a_}->{b}": n for (a_, b), n in sorted(moves.items())}
    silent = moves[("edge", "absent")] + moves[("unresolved", "absent")]
    print("call sites, v1 -> hybrid: " + ", ".join(f"{a_} -> {b} {n}" for (a_, b), n in sorted(moves.items())))
    print(f"  declared: {moves[('edge', 'unresolved')]} sites lost their edge and kept an unresolved_calls row; "
          f"silent: {silent} sites with neither edge nor row")
    # An edge or a row whose caller is no symbol is invisible to every
    # query that joins `symbols`: what a silent site above may hold.
    orphans = {}
    for arm, db in (("v1", a.v1), ("hybrid", a.hybrid)):
        con = connect(db)
        orphans[arm] = dict(
            edges=con.execute("SELECT count(*) FROM edges WHERE src_id NOT IN (SELECT id FROM symbols)").fetchone()[0],
            unresolved=con.execute("""SELECT count(*) FROM unresolved_calls WHERE enclosing_symbol_id IS NOT NULL
                                       AND enclosing_symbol_id NOT IN (SELECT id FROM symbols)""").fetchone()[0])
    merged = connect(a.hybrid).execute(
        """SELECT count(*) FROM ruby_hybrid_edges WHERE op = 'added'
            AND src_id NOT IN (SELECT id FROM symbols)""").fetchone()[0]
    summary["orphans"] = dict(orphans, merge_added_from_missing_caller=merged)
    print(f"  callers that are no symbol: v1 {orphans['v1']['edges']} edges, {orphans['v1']['unresolved']} "
          f"unresolved rows; hybrid {orphans['hybrid']['edges']} edges, {orphans['hybrid']['unresolved']} rows, "
          f"{merged} of the edges written by the merge")
    if silent:
        failed.append(f"{silent} call sites vanish silently ({merged} merge edges from a caller that is no symbol)")
    summary["unresolved_rows"] = dict(v1=sum(uv.values()), hybrid=sum(uh.values()))

    for name, holds, evidence in mvp_cases(a.mvp, a.repo, eh, ev):
        print(f"[{'PASS' if holds else 'OPEN'}] {name}: {evidence}")
        summary.setdefault("cases", []).append(dict(case=name, holds=holds, evidence=evidence))
        if not holds:
            failed.append(name)

    if a.judgments:
        judged = {}
        for sample in ("sample-v1-only", "sample-hybrid-only", "sample-raised", "sample-routes-added"):
            rows, got, missing = verdicts(a.out, a.judgments, sample)
            c = collections.Counter(got.get(r["id"], "missing") for r in rows)
            judged[sample] = dict(rows=len(rows), **c)
            wrong = [r["id"] for r in rows if got.get(r["id"]) in ("wrong", "undecidable")]
            print(f"{sample}: {len(rows)} rows, {c['correct']} correct, {c['wrong']} wrong, "
                  f"{c['undecidable']} undecidable, {len(missing)} without a verdict")
            if missing:
                failed.append(f"{sample}: {len(missing)} rows without a verdict")
            if sample != "sample-v1-only" and wrong:
                failed.append(f"{sample}: wrong targets the merge wrote: {', '.join(wrong)}")
        summary["judged"] = judged
        # v1's level: pixel-research-mvp's 2026-10-15 sample of 80 v1 call edges.
        base = {r["id"]: r for r in csv.DictReader(open(os.path.join(here, "v1-sample.tsv")), delimiter="\t")}
        bv = {}
        for path in glob.glob(os.path.join(a.judgments, "v1-*.tsv")):
            for line in open(path):
                p = line.rstrip("\n").split("\t")
                if len(p) >= 2 and p[0] in base:
                    bv[p[0]] = p[1].strip().lower()
        ids_v = {(r["caller_path"], str(r["line"]), r["callee_path"], r["callee"])
                 for k in ("call", "call-self") for r in ev.get(k, {}).values()}
        ids_h = {(r["caller_path"], str(r["line"]), r["callee_path"], r["callee"])
                 for k in ("call", "call-self") for r in eh.get(k, {}).values()}
        still = sum((r["caller_path"], r["line"], r["callee_path"], r["callee"]) in ids_v
                    and (r["caller_path"], r["line"], r["callee_path"], r["callee"]) in ids_h
                    for r in base.values())
        k_ok = sum(v == "correct" for v in bv.values())
        n = len(bv)
        lo, hi = wilson(k_ok, n)
        nv = summary["call"]["v1"] + summary["call-self"]["v1"]
        nh = summary["call"]["hybrid"] + summary["call-self"]["hybrid"]
        dropped, added = calls["v1-only"], calls["hybrid-only"]
        ok = lambda s: judged[s].get("correct", 0)
        rate = ok("sample-v1-only") / judged["sample-v1-only"]["rows"] if dropped else 0.0
        gone_ok = len(dropped) * rate
        new_rate = ok("sample-hybrid-only") / judged["sample-hybrid-only"]["rows"] if added else 0.0
        nlo, nhi = wilson(ok("sample-hybrid-only"), judged["sample-hybrid-only"]["rows"])
        v1_ok = nv * k_ok / n
        hy_ok = v1_ok - gone_ok + len(added) * new_rate
        est = dict(v1_sample=f"{k_ok}/{n}", v1_sample_in_both_graphs=still, v1_edges=nv, hybrid_edges=nh,
                   v1_correct=round(v1_ok), v1_interval=[round(nv * lo), round(nv * hi)],
                   dropped=len(dropped), dropped_correct=round(gone_ok),
                   added=len(added), added_precision=[round(new_rate, 3), round(nlo, 3), round(nhi, 3)],
                   hybrid_correct=round(hy_ok), difference=round(hy_ok - v1_ok),
                   difference_interval=[round(-gone_ok + len(added) * nlo), round(-gone_ok + len(added) * nhi)],
                   v1_precision=round(v1_ok / nv, 4), hybrid_precision=round(hy_ok / nh, 4))
        summary["estimate"] = est
        print("estimate: " + json.dumps(est))
        if hy_ok < v1_ok or hy_ok / nh < v1_ok / nv:
            failed.append("estimated correct call edges or precision below v1's")
    json.dump(summary, open(os.path.join(a.out, "summary.json"), "w"), indent=1)
    print(f"verdict: {'HOLDS' if not failed else 'OPEN'}" + "".join(f"\n  - {f}" for f in failed))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
