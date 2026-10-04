#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Select a fixed real-request corpus from retained GitHub API captures."""
import argparse
import hashlib
import json
from pathlib import Path

REPOSITORIES = ['BurntSushi/ripgrep', 'astral-sh/ruff', 'excalidraw/excalidraw']


def select(captures, per_repo=15):
    rows, exclusions = [], []
    for repo in REPOSITORIES:
        slug = repo.replace('/', '--')
        pin = json.loads((captures / (slug + '-pin.json')).read_text())
        metadata = json.loads((captures / (slug + '-repository.json')).read_text())
        if metadata['fork']:
            raise ValueError('fork requires explicit split grouping: ' + repo)
        records = {}
        for path in sorted(captures.glob(slug + '-issues*.json')):
            for issue in json.loads(path.read_text()):
                if issue['number'] in records:
                    raise ValueError('issue changed during paginated capture; recollect: ' + repo)
                records[issue['number']] = issue
        eligible = []
        for issue in sorted(records.values(), key=lambda r: (r['created_at'], r['number']), reverse=True):
            text = issue['title'] + '\n\n' + (issue.get('body') or '')
            reason = 'pull-request' if 'pull_request' in issue else ('length' if not 40 <= len(text) <= 6000 else None)
            if reason:
                exclusions.append({'repository': repo, 'number': issue['number'], 'reason': reason, 'characters': len(text)})
                continue
            eligible.append((issue, text))
        if len(eligible) < per_repo:
            raise ValueError('insufficient eligible requests: ' + repo)
        for issue, text in eligible[:per_repo]:
            rows.append(dict(id=f"{slug}-{issue['number']}", repository=repo,
                commit=pin['commit'], snapshot_role='collection context, not issue-introducing commit',
                url=issue['html_url'], number=issue['number'], created_at=issue['created_at'],
                updated_at=issue['updated_at'], text=text,
                text_sha256=hashlib.sha256(text.encode()).hexdigest(), split='test',
                provenance='verbatim public GitHub issue title plus body; not rewritten'))
        exclusions.extend({'repository': repo, 'number': i['number'], 'reason': 'beyond-fixed-count'}
                          for i, _ in eligible[per_repo:])
    normalized = [' '.join(r['text'].lower().split()) for r in rows]
    if len(set(normalized)) != len(rows):
        raise ValueError('exact duplicate requests require family adjudication')
    return {'requests': rows, 'exclusions': exclusions, 'selection': 'newest 15 eligible issues per repository'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--captures', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = select(args.captures)
    with args.output.open('x') as stream:
        json.dump(result, stream, indent=2)
        stream.write('\n')
    print('Selected', len(result['requests']), 'requests; exclusions', len(result['exclusions']))


if __name__ == '__main__':
    main()
