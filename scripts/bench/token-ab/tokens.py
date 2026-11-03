# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

# Per-arm token breakdown of one `arena.sh run-set --agent claude` set, read from each arm's transcript;
# asserts the sum equals the receipt's total_tokens.
import json, sys, glob, os, collections
d=sys.argv[1]; K=('input_tokens','cache_creation_input_tokens','cache_read_input_tokens','output_tokens')
tot={a:collections.Counter() for a in ('pixel','no-pixel')}; seen_roll=collections.Counter(); rows=[]
for f in sorted(glob.glob(d+'/*.receipt.json')):
    r=json.load(open(f)); row=[r['case'] if isinstance(r.get('case'),str) else os.path.basename(f).split('.')[0]]
    for a in ('no-pixel','pixel'):
        arm=r['arms'][a]; p=arm.get('rollout'); seen_roll[p]+=1
        last={}
        for line in open(p):
            try: e=json.loads(line)
            except: continue
            m=e.get('message') or {}
            if e.get('type')=='assistant' and m.get('usage'): last[m.get('id')]=m['usage']
        c=collections.Counter({k:sum(u.get(k,0) or 0 for u in last.values()) for k in K})
        c['calls']=len(arm.get('tool_calls') or []); c['total']=sum(c[k] for k in K); c['turns']=len(last)
        assert c['total']==(arm.get('tokens') or {}).get('total_tokens'), (f,a,c['total'],arm.get('tokens'))
        tot[a]+=c; row.append(c)
    rows.append(row)
assert max(seen_roll.values())==1, 'shared transcript across cases'
print(f"{'case':16}{'no-pixel total':>15}{'pixel total':>13}{'delta':>8}   fresh(in+write+out) np→px")
for name,np,px in rows:
    fr=lambda c:c['input_tokens']+c['cache_creation_input_tokens']+c['output_tokens']
    print(f"{name:16}{np['total']:15,}{px['total']:13,}{100*(px['total']/np['total']-1):+7.0f}%   {fr(np):,}→{fr(px):,}")
for a in ('no-pixel','pixel'): print(a, dict(tot[a]))
np,px=tot['no-pixel'],tot['pixel']
for k in K+('total','calls','turns'): print(f"{k:30}{np[k]:12,}{px[k]:12,}{100*(px[k]/max(np[k],1)-1):+7.1f}%")
w={'input_tokens':1,'cache_creation_input_tokens':1.25,'cache_read_input_tokens':0.1,'output_tokens':5}
cost=lambda c:sum(c[k]*w[k] for k in K)
print(f"price-weighted (input=1, write=1.25, read=0.1, output=5): {cost(np):,.0f} -> {cost(px):,.0f} ({100*(cost(px)/cost(np)-1):+.1f}%)")
