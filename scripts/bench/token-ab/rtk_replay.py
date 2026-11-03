# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Replays every Bash call of the no-pixel arm raw and through `rtk rewrite`, in the
# arm's own clone, and simulates an RTK arm: same agent behaviour, smaller tool output.
# shortcut: assumes RTK changes no agent decision (favours RTK), upgrade to a live RTK arm if the margin is thin.
import json, sys, glob, os, subprocess, collections, tempfile
# A fresh HOME: RTK's default filters, and the replay stays out of the user's `rtk gain`.
ENV=dict(os.environ, HOME=tempfile.mkdtemp(prefix='rtk-home-')); ENV.pop('ANTHROPIC_API_KEY',None)
def run(cmd, cwd):
    try: p=subprocess.run(['bash','-c',cmd],cwd=cwd,env=ENV,capture_output=True,timeout=60); return len(p.stdout)+len(p.stderr)
    except subprocess.TimeoutExpired: return None
def blocks(p):
    out=[]  # (kind, id/usage) in order
    for line in open(p):
        try: e=json.loads(line)
        except: continue
        m=e.get('message') or {}
        for b in m.get('content') if isinstance(m.get('content'),list) else []:
            if b.get('type')=='tool_use': out.append(('use',b['id'],b))
            if b.get('type')=='tool_result': out.append(('res',b['tool_use_id'],b))
        if e.get('type')=='assistant' and m.get('usage'): out.append(('api',m.get('id'),m['usage']))
    return out
K=('input_tokens','cache_creation_input_tokens','cache_read_input_tokens','output_tokens')
G=collections.Counter(); per=[]
for d in sys.argv[1:]:
  for f in sorted(glob.glob(d+'/*.receipt.json')):
    r=json.load(open(f)); a=r['arms']['no-pixel']; seq=blocks(a['rollout'])
    ws=next(json.loads(l)['cwd'] for l in open(a['rollout']) if '"cwd"' in l)  # panes are reused, so the clone is the session's cwd
    api_ids=[]; [api_ids.append(i) for k,i,_ in seq if k=='api' and i not in api_ids]
    usage={i:u for k,i,u in seq if k=='api'}
    total=sum(sum(u.get(k,0) or 0 for k in K) for u in usage.values())
    uses={i:b for k,i,b in seq if k=='use'}; saved_ctx=0; raw_b=rtk_b=0
    for idx,(k,i,b) in enumerate(seq):
        if k!='res' or uses.get(i,{}).get('name')!='Bash': continue
        cmd=uses[i]['input'].get('command','')
        rw=subprocess.run(['rtk','rewrite',cmd],env=ENV,capture_output=True,text=True)
        new=rw.stdout.strip() if rw.returncode in (0,3) and rw.stdout.strip() else cmd
        rb=run(cmd,ws); nb=run(new,ws) if new!=cmd else rb
        if rb is None or nb is None: continue
        raw_b+=rb; rtk_b+=nb
        later=len({j for kk,j,_ in seq[idx:] if kk=='api'})  # API calls that carry this result
        saved_ctx+=max(rb-nb,0)/4*later
    per.append((r['case'] if isinstance(r['case'],str) else os.path.basename(f), total, saved_ctx, raw_b, rtk_b))
    G['total']+=total; G['saved']+=saved_ctx; G['raw']+=raw_b; G['rtk']+=rtk_b
for c,t,s,rb,nb in per: print(f'{c:16} base {t:9,} rtk-sim {t-s:11,.0f} ({-100*s/t:+5.1f}%)  bash out {rb:7,}B -> {nb:7,}B')
print(f"ALL base {G['total']:,} rtk-sim {G['total']-G['saved']:,.0f} ({-100*G['saved']/G['total']:+.1f}%), bash output {G['raw']:,}B -> {G['rtk']:,}B ({100*(G['rtk']/max(G['raw'],1)-1):+.1f}%)")
