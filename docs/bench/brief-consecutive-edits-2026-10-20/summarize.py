# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
import collections, hashlib, json, math, pathlib, statistics, sys
root=pathlib.Path(sys.argv[1])
rows=[json.loads(s) for s in (root/'events.jsonl').read_text().splitlines()]
summary={}
for arm in ['v1','hybrid']:
 hooks=[r for r in rows if r['phase']=='hook' and r['arm']==arm and r['k']>=0]
 early=[r for r in hooks if r['k']%3==0]
 later=[r for r in hooks if r['k']%3!=0]
 def stats(rs):
  lat=sorted(r['ms'] for r in rs)
  return {'n':len(rs),'content':sum((r.get('bytes') or 0)>0 for r in rs),
   'answered':sum((r.get('answered') or 0)>0 for r in rs),
   'p50_ms':lat[max(0,math.ceil(len(lat)*.5)-1)] if lat else None,
   'p95_ms':lat[max(0,math.ceil(len(lat)*.95)-1)] if lat else None,
   'max_ms':max(lat,default=None),'routes':dict(collections.Counter(r.get('route') for r in rs)),
   'stale_notice':sum('staleness: some daemon evidence predates the last edit;' in (root/f'{arm}-{r["k"]}.stdout').read_text() for r in rs),
   'outputs':{str(r['k']):hashlib.sha256((root/f'{arm}-{r["k"]}.stdout').read_bytes()).hexdigest() for r in rs}}
 pubs=[r for r in rows if r['phase']=='published' and r['arm']==arm]
 summary[arm]={'early':stats(early),'later':stats(later),'published':sum(r['seen'] for r in pubs),'publication_checks':len(pubs)}
summary['load1']={'min':min(r['load1'] for r in rows),'median':statistics.median(r['load1'] for r in rows),'max':max(r['load1'] for r in rows)}
(root/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
for arm in ['v1','hybrid']:
 print(arm, {k:{i:v for i,v in x.items() if i!='outputs'} if isinstance(x,dict) else x for k,x in summary[arm].items()})
print('load1',summary['load1'])
