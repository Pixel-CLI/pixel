# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Codex variant of rtk_replay.py: same simulation, commands parsed from exec_command({cmd:"..."}) calls.
# shortcut: plugin-skill reads (plugins/cache) are replayed like any command, as RTK would see them.
import json, sys, glob, os, re, subprocess, tempfile
# A fresh HOME: RTK's default filters, and the replay stays out of the user's `rtk gain`.
ENV=dict(os.environ, HOME=tempfile.mkdtemp(prefix='rtk-home-')); ENV.pop('ANTHROPIC_API_KEY',None)
CMD=re.compile(r'cmd:\s*("(?:[^"\\]|\\.)*")'); WD=re.compile(r'workdir:\s*("(?:[^"\\]|\\.)*")')
def size(cmd,cwd):
    try: p=subprocess.run(['bash','-c',cmd],cwd=cwd,env=ENV,capture_output=True,timeout=60); return len(p.stdout)+len(p.stderr)
    except subprocess.TimeoutExpired: return None
tot=saved=raw=new=0
for d in sys.argv[1:]:
  for f in sorted(glob.glob(d+'/*.receipt.json')):
    r=json.load(open(f)); ev=[json.loads(l) for l in open(r['arms']['no-pixel']['rollout'])]
    cwd=next(e['payload']['cwd'] for e in ev if e.get('type')=='session_meta')
    tot+=r['arms']['no-pixel']['tokens']['total_tokens']
    for i,e in enumerate(ev):
        p=e.get('payload') or {}
        if p.get('type')!='custom_tool_call': continue
        later=sum(1 for x in ev[i:] if (x.get('payload') or {}).get('type')=='token_count')
        for m in CMD.finditer(p.get('input') or ''):
            cmd=json.loads(m.group(1)); w=WD.search(p['input'][m.end():m.end()+300]); wd=json.loads(w.group(1)) if w else cwd
            rw=subprocess.run(['rtk','rewrite',cmd],env=ENV,capture_output=True,text=True)
            alt=rw.stdout.strip() if rw.returncode in (0,3) and rw.stdout.strip() else cmd
            a=size(cmd,wd); b=size(alt,wd) if alt!=cmd else a
            if a is None or b is None: continue
            raw+=a; new+=b; saved+=max(a-b,0)/4*later
print(f"codex base {tot:,} rtk-sim {tot-saved:,.0f} ({-100*saved/tot:+.1f}%), commands output {raw:,}B -> {new:,}B ({100*(new/max(raw,1)-1):+.1f}%)")
