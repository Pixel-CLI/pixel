#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Replay the historical hook phase; args: HARNESS BINARY V1 HYBRID OUT CYCLES.
The historical module is executed without its trailing main() call. Its
prepare, prompt generation, edits, environment and hook order are preserved.
Freshness timing campaigns belong to #1204; check publication after each cycle.
"""
import hashlib, json, pathlib, sqlite3, sys, time
harness = pathlib.Path(sys.argv.pop(1)).resolve()
source = harness.read_text()
assert source.endswith('main()\n')
ns = {'__file__': str(harness), '__name__': 'historical_bench'}
exec(compile(source.removesuffix('main()\n'), str(harness), 'exec'), ns)
roots, out, cycles = ns['roots'], ns['out'], ns['cycles']
edits = ['app/models/link.rb', 'app/models/purchase.rb', 'app/models/user.rb',
         'app/models/subscription.rb', 'app/controllers/links_controller.rb']
assert not (out / 'identity.json').exists(), 'use a new output directory'
(out / 'identity.json').write_text(json.dumps({'argv':sys.argv,
    'harness_sha256': hashlib.sha256(source.encode()).hexdigest(),
    'binary_sha256':hashlib.sha256(pathlib.Path(ns['binary']).read_bytes()).hexdigest(),
    'cycles':cycles}, indent=2))
failures = []
def hook(arm, prompt, k):
    payload = json.dumps({'session_id':f'bench-{arm}', 'cwd':str(roots[arm]),
        'hook_event_name':'UserPromptSubmit', 'prompt':prompt})
    before = int(time.time()*1000)
    p, secs = ns['pixel'](arm, 'run-hook', 'task-event', '--provider','claude',
                          '--event','prompt-submit',stdin=payload,timeout=60)
    (out / f'{arm}-{k}.stdout').write_text(p.stdout)
    (out / f'{arm}-{k}.stderr').write_text(p.stderr)
    decision = ns['last_decision'](arm,before-50)
    ok = p.returncode == 0 and (decision.get('bytes') or 0)>0 and (decision.get('answered') or 0)>0
    ns['emit'](phase='hook',arm=arm,k=k,ms=round(secs*1000,1),rc=p.returncode,
        out_bytes=len(p.stdout),passed=ok,**decision)
    if k>=0 and not ok: failures.append(f'{arm}/{k}: no answered brief')
    print(f'{arm} k={k}: {"PASS" if ok else "FAIL"} {secs*1000:.0f}ms {decision}',flush=True)
try:
    for arm in roots:
        ns['prepare'](arm)
        assert ns['pids'][arm], f'{arm}: daemon missing'
    time.sleep(3)
    for arm in roots: hook(arm,'Where is the refund of a purchase computed?',-1)
    prompts = ns['prompts'](3*cycles)
    for c in range(cycles):
        for arm in (list(roots) if c%2==0 else list(roots)[::-1]):
            name = f'hook_probe_{c}_{2*c + (list(roots) if c%2==0 else list(roots)[::-1]).index(arm)}'
            ns['insert_method'](roots[arm],edits[c%len(edits)],name)
            ns['emit'](phase='edit',arm=arm,c=c,file=edits[c%len(edits)])
            time.sleep(1)
            for j in range(3):
                hook(arm,prompts[3*c+j],3*c+j)
                time.sleep(2)
            # Observe only once, after the timed phase: prove the watcher ran.
            with sqlite3.connect(f'file:{roots[arm]}/.pixel/graph.v2.db?mode=ro',uri=True) as db:
                seen = bool(db.execute('SELECT 1 FROM symbols WHERE name=? LIMIT 1',(name,)).fetchone())
            ns['emit'](phase='published',arm=arm,c=c,seen=seen)
            if not seen: failures.append(f'{arm}/{c}: edit not published after cycle')
finally:
    for arm in roots: ns['pixel'](arm,'daemon','stop','.')
    ns['log'].close()
print(json.dumps({'failures':failures,'passed':not failures}),flush=True)
sys.exit(bool(failures))
