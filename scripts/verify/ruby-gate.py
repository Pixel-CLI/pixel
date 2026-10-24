#!/usr/bin/env python3
# SPDX-FileCopyrightText: The RTO contributors
# SPDX-License-Identifier: MIT
"""Ruby latency gate (specs/graph.md): on a large Rails repository, after an edit
- a brief arrives within 1 s (20 of 20),
- the PostToolUse hook answers in <= 100 ms (p95),
- the graph reflects the edit within 10 s (p95).
Usage: ruby-gate.py <repo> <pixel> [n]

The brief is asked one second after the edit, while the graph is still being
refreshed: it must arrive (non-empty, within a second) from the generation before
the edit, not wait for the new one. Then the script polls `find-symbol` for the
symbol the edit added, and times one `pixel hook post-tool` call. The edits are
undone with `git checkout`; run it on a throwaway clone."""
import json, os, random, statistics, subprocess, sys, time, pathlib

repo, pixel = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 20
env = dict(os.environ)
def run(args, **kw):
    return subprocess.run([pixel, *args], cwd=repo, env=env, capture_output=True, text=True, **kw)

run(['daemon', 'start', '.'])
time.sleep(3)
files = [l for l in subprocess.run(['git', 'ls-files', 'app/**/*.rb', 'app/*.rb'], cwd=repo, capture_output=True, text=True).stdout.split('\n') if l.endswith('.rb')]
random.seed(7)
random.shuffle(files)
picked = []
for f in files:
    text = pathlib.Path(repo, f).read_text(errors='ignore')
    if text.rstrip().endswith('end') and len(text) < 20000 and 'class ' in text:
        picked.append(f)
    if len(picked) == n: break
rows = []
for i, f in enumerate(picked):
    path = pathlib.Path(repo, f)
    original = path.read_text()
    name = f'pixel_gate_probe_{i}'
    body = original.rstrip()
    assert body.endswith('end')
    edited = body[:-3] + f'\n  def {name}\n    42\n  end\nend\n'
    t0 = time.perf_counter()
    path.write_text(edited)
    # a brief one second after the edit, while the graph is refreshing
    time.sleep(1.0)
    t1 = time.perf_counter()
    b = run(['brief', f'what does {name} do and where is it defined', '.', '--json', '--metrics', 'off'])
    brief_s = time.perf_counter() - t1
    try:
        brief_bytes = json.loads(b.stdout)['record']['bytes']
    except Exception:
        brief_bytes = -1
    # graph freshness: the new symbol is found
    seen = None
    while time.perf_counter() - t0 < 30:
        out = run(['find-symbol', name, '.', '--json', '--metrics', 'off'])
        try:
            if json.loads(out.stdout).get('symbols'):
                seen = time.perf_counter() - t0
                break
        except Exception:
            pass
        time.sleep(0.05)
    # PostToolUse hook latency
    payload = json.dumps({'hook_event_name': 'PostToolUse', 'tool_name': 'Bash', 'tool_input': {'command': 'ls'}, 'tool_response': {}, 'session_id': 'gate', 'cwd': repo})
    t2 = time.perf_counter()
    subprocess.run([pixel, 'hook', 'post-tool', '--host', 'claude'], cwd=repo, env=env, input=payload, capture_output=True, text=True)
    hook_ms = (time.perf_counter() - t2) * 1000
    path.write_text(original)
    rows.append((f, seen, brief_s, brief_bytes, hook_ms))
    print(f'{f:60s} graph={seen if seen is None else round(seen,2)}s brief={brief_s:.2f}s ({brief_bytes}B) hook={hook_ms:.0f}ms', flush=True)
subprocess.run(['git', 'checkout', '--', '.'], cwd=repo)
def p95(xs):
    xs = sorted(xs); return xs[min(len(xs)-1, int(round(0.95*(len(xs)-1))))]
graph = [r[1] for r in rows if r[1] is not None]
print(f'edits={len(rows)} graph seen={len(graph)}/{len(rows)} p95={p95(graph):.2f}s' if graph else 'graph never fresh')
print(f'briefs within 1s: {sum(1 for r in rows if r[2] <= 1.0 and r[3] > 0)}/{len(rows)}; max {max(r[2] for r in rows):.2f}s')
print(f'PostToolUse p95 {p95([r[4] for r in rows]):.0f} ms, median {statistics.median([r[4] for r in rows]):.0f} ms')
