#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT
"""Record current PR heads and verify the linked CI run's identity."""
import argparse
import json
import re
import subprocess
from pathlib import Path

parser=argparse.ArgumentParser(); parser.add_argument('prs',nargs='+',type=int)
parser.add_argument('--out',required=True); args=parser.parse_args()
def gh(*parts):
    return json.loads(subprocess.run(['rtk','gh',*parts],check=True,capture_output=True,text=True).stdout)
rows=[]
for number in args.prs:
    pr=gh('pr','view',str(number),'--repo','Pixel-CLI/pixel','--json',
          'number,url,state,headRefName,headRefOid,baseRefName,statusCheckRollup,reviewDecision,comments,labels')
    runs={}
    for check in pr['statusCheckRollup']:
        if check.get('workflowName')!='CI':
            continue
        match=re.search(r'/actions/runs/(\d+)',check.get('detailsUrl',''))
        if match and match.group(1) not in runs:
            run=gh('run','view',match.group(1),'--repo','Pixel-CLI/pixel','--json',
                   'headSha,event,status,conclusion,displayTitle,url')
            runs[match.group(1)]=run
    passing=bool(runs) and all(run['headSha']==pr['headRefOid'] and run['status']=='completed'
                              and run['conclusion']=='success' for run in runs.values())
    bad=[check.get('name') for check in pr['statusCheckRollup']
         if check.get('status')!='COMPLETED' or check.get('conclusion') not in ('SUCCESS','SKIPPED','NEUTRAL')]
    passing=passing and not bad
    rows.append({'pr':pr,'ci_runs':runs,'current_head_checks_pass':passing,'nonpassing_checks':bad})
    print(f'PR {number}: {pr["headRefOid"]} current_head_checks_pass={passing} checks={len(pr["statusCheckRollup"])}')
Path(args.out).write_text(json.dumps(rows,indent=2)+'\n')
print(f'Current-head CI: {sum(row["current_head_checks_pass"] for row in rows)}/{len(rows)}')
raise SystemExit(0 if all(row['current_head_checks_pass'] for row in rows) else 1)
