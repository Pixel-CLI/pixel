#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""What a user waits for with v1's Ruby graph source off (v1) and on (hybrid),
the daemon running as `pixel prepare-repo` leaves it.

usage: daemonbench.py BINARY V1_CLONE HYBRID_CLONE OUT_DIR [CYCLES] [EDITS...]

Both clones are the same commit. Each arm has its own HOME/XDG (env.sh) and
its own daemon, started by `prepare-repo`; the hybrid arm's processes all
carry PIXEL_GRAPH_SOURCE_RUBY=hybrid, the v1 arm's none.

Phase 1, hook: CYCLES cycles per arm, the arms interleaved (ABBA by cycle).
A cycle: a one-file edit (a 3-line method inserted mid-file, the files of
EDITS in rotation), 1.0 s, prompt, 2.0 s, prompt, 2.0 s, prompt, 2.0 s.
Every prompt runs the installed hook command
`pixel run-hook task-event --provider claude --event prompt-submit` with a
Claude UserPromptSubmit payload; its wall time is the hook's latency, and the
brief's own record (.pixel/brief-decisions.jsonl) says which route answered
and how many of its operations did.

Phase 2, freshness: CYCLES edits per arm, interleaved; after each write,
(a) passive: the graph database polled read-only every 25 ms until the new
method is a symbol; then CYCLES more edits, (b) active: `pixel find-symbol`
on the new method run right after the write, again until it answers it.

A sampler reads both daemons' RSS and CPU time every 100 ms throughout.
"""
import json, os, pathlib, random, re, sqlite3, subprocess, sys, threading, time

HERE = pathlib.Path(__file__).resolve().parent
ENV_SH = str(HERE / "env.sh")
binary = sys.argv[1]
roots = {"v1": pathlib.Path(sys.argv[2]).resolve(), "hybrid": pathlib.Path(sys.argv[3]).resolve()}
out = pathlib.Path(sys.argv[4]).resolve(); out.mkdir(parents=True, exist_ok=True)
cycles = int(sys.argv[5]) if len(sys.argv) > 5 else 20
edits = sys.argv[6:] or ["app/models/link.rb"]
homes = {arm: out / f"home-{arm}" for arm in roots}
log = open(out / "events.jsonl", "a")


def emit(**row):
    row["t"] = time.time()
    row["load1"] = os.getloadavg()[0]
    log.write(json.dumps(row) + "\n"); log.flush()


def env_for(arm):
    env = dict(os.environ)
    env.pop("PIXEL_GRAPH_SOURCE_RUBY", None)
    if arm == "hybrid":
        env["PIXEL_GRAPH_SOURCE_RUBY"] = "hybrid"
    return env


def pixel(arm, *args, stdin=None, timeout=900):
    t0 = time.monotonic()
    p = subprocess.run([ENV_SH, str(homes[arm]), binary, *args], cwd=roots[arm], env=env_for(arm),
                       input=stdin, capture_output=True, text=True, timeout=timeout)
    return p, time.monotonic() - t0


def du_kb(path):
    if not path.exists():
        return 0
    return int(subprocess.run(["du", "-sk", str(path)], capture_output=True, text=True).stdout.split()[0])


def sizes(arm):
    d = roots[arm] / ".pixel"
    return {"pixel_kb": du_kb(d), "graph_kb": sum(du_kb(p) for p in d.glob("graph.v2.db*")),
            "mvp_kb": du_kb(d / ".pixel-mvp")}


def daemon_pid(arm):
    ps = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    for line in ps.splitlines():
        pid, _, cmd = line.strip().partition(" ")
        if "daemon start" in cmd and cmd.split("daemon start ", 1)[1].split(" --")[0] in (
                str(roots[arm]), str(roots[arm]).replace("/private", "", 1),
                "/private" + str(roots[arm])):
            return int(pid)
    return None


# --- sampler ---------------------------------------------------------------
samples = []  # (t, arm, rss_kb, cpu_s)
pids = {}
stop = threading.Event()


def cputime(text):
    parts = text.split(":")
    secs = float(parts[-1]) + 60 * int(parts[-2])
    if len(parts) == 3:
        secs += 3600 * int(parts[0])
    return secs


def sampler():
    while not stop.is_set():
        live = {pid: arm for arm, pid in pids.items() if pid}
        if live:
            r = subprocess.run(["ps", "-o", "pid=,rss=,time=", "-p", ",".join(map(str, live))],
                               capture_output=True, text=True).stdout
            now = time.time()
            for line in r.splitlines():
                pid, rss, cpu = line.split()
                samples.append((now, live[int(pid)], int(rss), cputime(cpu)))
        stop.wait(0.1)


def window_stats(arm, t0, t1):
    rows = [s for s in samples if s[1] == arm and t0 <= s[0] <= t1]
    if len(rows) < 2:
        return None
    return {"rss_max_kb": max(r[2] for r in rows), "cpu_s": round(rows[-1][3] - rows[0][3], 2),
            "peak_cpu_pct": round(max((b[3] - a[3]) / (b[0] - a[0]) * 100 for a, b in zip(rows, rows[1:])
                                      if b[0] > a[0]), 0)}


def db_stamp(db):
    out_ = []
    for p in (db, db.with_name(db.name + "-wal")):
        try:
            st = os.stat(p)
            out_.append((st.st_ino, st.st_size, st.st_mtime_ns))
        except OSError:
            out_.append(None)
    return out_


# --- edits -----------------------------------------------------------------
def insert_method(root, rel, name):
    p = root / rel
    lines = p.read_text().split("\n")
    at = len(lines) // 2
    for j in range(at, len(lines)):
        s = lines[j].lstrip()
        if s.startswith("def "):
            ind = lines[j][: len(lines[j]) - len(s)]
            lines[j:j] = [f"{ind}def {name}", f"{ind}  find_by_external_id(1)", f"{ind}end", ""]
            break
    else:
        raise SystemExit(f"no def after the middle of {rel}")
    p.write_text("\n".join(lines))


# --- prompts ---------------------------------------------------------------
def vocabulary(root):
    pairs = []
    for f in sorted((root / "app" / "models").rglob("*.rb")):
        text = f.read_text(errors="replace")
        m = re.search(r"^class (\w+)", text, re.M)
        if not m:
            continue
        for d in sorted(set(re.findall(r"^\s+def (?:self\.)?([a-z_]\w*[?!]?)", text, re.M))):
            pairs.append((m.group(1), d))
    return pairs


TEMPLATES = [
    "Why does {C}#{m} return the wrong value after {C2}#{m2} runs? Find the bug and fix it.",
    "Add a test for {C}#{m} covering the case where the {C2} is missing.",
    "Where is {m} called from, and what breaks if I rename it to {m}_v2?",
    "Refactor {C}#{m} to reuse {C2}#{m2} instead of duplicating the logic.",
    "Explain how {C} and {C2} interact when {m2} is called on a {C2}.",
]


def prompts(n):
    pairs = vocabulary(roots["v1"])
    rng = random.Random(20261016)
    out_ = []
    for _ in range(n):
        (c, m), (c2, m2) = rng.sample(pairs, 2)
        out_.append(rng.choice(TEMPLATES).format(C=c, m=m, C2=c2, m2=m2))
    return out_


def last_decision(arm, after_ms):
    f = roots[arm] / ".pixel" / "brief-decisions.jsonl"
    if not f.exists():
        return {}
    for line in reversed(f.read_text().splitlines()):
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("ts", 0) >= after_ms:
            return {k: row.get(k) for k in ("route", "ops", "answered", "elapsed_ms", "bytes", "kind", "fallback",
                                            "reason", "daemon")}
        break
    return {}


def hook(arm, prompt, k):
    payload = json.dumps({"session_id": f"bench-{arm}", "cwd": str(roots[arm]),
                          "hook_event_name": "UserPromptSubmit", "prompt": prompt})
    before = int(time.time() * 1000)
    p, secs = pixel(arm, "run-hook", "task-event", "--provider", "claude", "--event", "prompt-submit",
                    stdin=payload, timeout=60)
    emit(phase="hook", arm=arm, k=k, ms=round(secs * 1000, 1), rc=p.returncode, out_bytes=len(p.stdout),
         **last_decision(arm, before - 50))


# --- run -------------------------------------------------------------------
def prepare(arm):
    subprocess.run(["git", "checkout", "-q", "--", "."], cwd=roots[arm], check=True)
    pixel(arm, "daemon", "stop", ".")
    subprocess.run(["rm", "-rf", str(roots[arm] / ".pixel"), str(homes[arm])], check=True)
    p, secs = pixel(arm, "--metrics", "off", "prepare-repo", ".", "--json")
    data = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {}
    pids[arm] = daemon_pid(arm)
    emit(phase="prepare", arm=arm, s=round(secs, 2), rc=p.returncode, timings=data.get("timings"),
         graph=data.get("graph"), daemon=data.get("daemon"), pid=pids[arm], **sizes(arm))


def main():
    threading.Thread(target=sampler, daemon=True).start()
    for arm in roots:
        prepare(arm)
    time.sleep(3)
    for arm in roots:  # warm: the first graph op opens the graph
        hook(arm, "Where is the refund of a purchase computed?", -1)
    ps = prompts(3 * cycles)
    order = list(roots)
    n = 0
    for c in range(cycles):
        arms = order if c % 2 == 0 else order[::-1]
        for arm in arms:
            rel = edits[c % len(edits)]
            t0 = time.time()
            insert_method(roots[arm], rel, f"hook_probe_{c}_{n}")
            emit(phase="edit", arm=arm, c=c, file=rel)
            time.sleep(1.0)
            for j in range(3):
                hook(arm, ps[3 * c + j], 3 * c + j)
                time.sleep(2.0)
            emit(phase="cycle", arm=arm, c=c, **(window_stats(arm, t0, time.time()) or {}))
            n += 1
    # Freshness, passive then active.
    for mode in ("passive", "active"):
        for c in range(cycles):
            arms = order if c % 2 == 0 else order[::-1]
            for arm in arms:
                rel = edits[c % len(edits)]
                name = f"fresh_{mode}_{c}"
                db = roots[arm] / ".pixel" / "graph.v2.db"
                time.sleep(1.5)
                t0 = time.time(); m0 = time.monotonic()
                insert_method(roots[arm], rel, name)
                calls = 0
                seen = None
                stamp = db_stamp(db)
                while time.monotonic() - m0 < 60:
                    if mode == "passive":
                        # Only stat until the database moves: opening it
                        # writes its -shm, and any event in the tree, even
                        # one the watcher ignores, pushes its debounce back
                        # while an edit is pending (daemon.rs `note_event`).
                        now_stamp = db_stamp(db)
                        if now_stamp == stamp:
                            time.sleep(0.01)
                            continue
                        stamp = now_stamp
                        try:
                            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=0.5)
                            hit = con.execute("SELECT 1 FROM symbols WHERE name = ? LIMIT 1", (name,)).fetchone()
                            con.close()
                        except sqlite3.Error:
                            hit = None
                        calls += 1
                        if hit:
                            seen = time.monotonic() - m0
                            break
                    else:
                        p, _ = pixel(arm, "--metrics", "off", "find-symbol", name, timeout=60)
                        calls += 1
                        if name in p.stdout:
                            seen = time.monotonic() - m0
                            break
                # let the update finish before the stats window closes
                time.sleep(0.5 if seen is None or seen > 0.5 else 1.0)
                emit(phase="fresh", mode=mode, arm=arm, c=c, file=rel,
                     s=None if seen is None else round(seen, 3), calls=calls,
                     **(window_stats(arm, t0, time.time()) or {}))
    for arm in roots:
        emit(phase="end", arm=arm, **sizes(arm))
    stop.set()
    for arm in roots:
        pixel(arm, "daemon", "stop", ".")
    with open(out / "samples.tsv", "w") as f:
        for s in samples:
            f.write("\t".join(map(str, s)) + "\n")


main()
