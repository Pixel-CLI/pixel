#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Extract narrow observed plan calls via Pixel recall, not transcript files."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess

CUTOFF_MS = 1790035200000  # 2026-09-22T00:00:00Z
OPTIONS = {'--query', '--tag', '--limit', '--format', '--max-todos', '--metrics'}
FLAGS = {'--json', '--no-verify'}
QUERIES = {'dead-interactive', 'dead-code', 'hotspots', 'recent-changes', 'by-concept'}
COMPLETION = re.compile(r'^\d+\. \[ \] (?:Verify all plan targets|Refactor hotspot file|No callers found)', re.M)


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def calls(text):
    found, excluded = [], []
    for marker in re.finditer(r'(?:^|\n)\u22eetool\s+(?:bash|Bash)\s+(\{)', text):
        args, _ = json.JSONDecoder().raw_decode(text[marker.start(1):])
        command = args.get('command', '')
        if not isinstance(command, str):
            continue
        if re.search(r'<<|\$\(|`|(?:^|[;\n])\s*(?:for|while|if|case|function)\b', command):
            excluded.append('heredoc_or_dynamic_script_not_parsed')
            continue
        lexer = shlex.shlex(command, posix=True, punctuation_chars=';|\n')
        lexer.whitespace = ' \t\r'
        lexer.whitespace_split = True
        tokens = list(lexer)
        segments, current = [], []
        for token in tokens + [';']:
            if token in [';', '&&', '||', '|', '&', '\n']:
                if current:
                    segments.append(current)
                current = []
            else:
                current.append(token)
        changed_cwd = False
        for segment in segments:
            if segment[0] == 'cd':
                changed_cwd = True
            while segment and (re.match(r'^[A-Za-z_][A-Za-z0-9_]*=', segment[0]) or segment[0] == 'env'):
                segment = segment[1:]
            if len(segment) < 2 or Path(segment[0]).name != 'pixel' or segment[1] != 'plan':
                continue
            if changed_cwd:
                excluded.append('shell_changed_cwd_not_resolved')
                continue
            argv = [s for s in segment if not re.fullmatch(r'\d*>&\d+', s)]
            if '--help' in argv or '-h' in argv:
                excluded.append('help_not_a_request')
                continue
            if any('$' in a for a in argv):
                excluded.append('nonliteral_argv_not_resolved')
                continue
            values, positional = {}, []
            index = 2
            while index < len(argv):
                token = argv[index]
                if token in OPTIONS:
                    if index + 1 == len(argv):
                        raise ValueError('missing option argument')
                    values[token] = argv[index+1]
                    index += 2
                elif token in FLAGS:
                    index += 1
                elif token.startswith('-'):
                    raise ValueError('unsupported option: ' + token)
                else:
                    positional.append(token)
                    index += 1
            query = values.get('--query')
            if query is not None and query not in QUERIES:
                excluded.append('invalid_explicit_query')
                continue
            if len(positional) > 2 or (not positional and query is None):
                excluded.append('ambiguous_or_missing_positional_input')
                continue
            # The historical directory existence is not guessed: only '.' is certain.
            prompt = positional[0] if positional else None
            path = positional[1] if len(positional) == 2 else '.'
            if query and query != 'by-concept' and positional == ['.']:
                prompt = None
            found.append({'argv': argv, 'prompt': prompt, 'explicit_query': query,
                          'path_argument': path, 'parameters': values})
    return found, excluded


def corroborating_actions(call, timestamp, cwd, actions):
    return [a['invocation_id'] for a in actions
            if a.get('invocation_id') and a['ts_ms'] < CUTOFF_MS
            and abs(a['ts_ms'] - timestamp) < 30000 and a['cwd'] == cwd
            and a['args'] == ' '.join(call['argv'][1:]) and a['outcome'] == 'ok']


def recall(session, turns):
    command = ['pixel', 'recall', 'show', str(session), '--turn', str(turns), '--json']
    result = subprocess.run(command, env={**os.environ, 'PIXEL_METRICS': '0'},
                            capture_output=True, text=True, check=True, timeout=60)
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--search', type=Path, required=True)
    parser.add_argument('--actions', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    hits = json.loads(args.search.read_text())
    if hits['truncated']:
        raise ValueError('search is capped; do not treat it as the complete returned population')
    actions = json.loads(args.actions.read_text())
    records, exclusions = [], []
    for hit in hits['hits']:
        ref = f"{hit['agent']}:{hit['source_session_id']}"
        identity = {'ref': ref, 'seq': hit['seq']}
        result = recall(hit['session_id'], hit['seq'])
        if result.get('truncated') or len(result['turns']) != 1 or result['turns'][0]['truncated']:
            exclusions.append(dict(identity, reason='capped_or_missing_turn'))
            continue
        turn = result['turns'][0]
        if turn['role'] != 'assistant' or turn['ts'] is None or turn['ts'] >= CUTOFF_MS:
            exclusions.append(dict(identity, reason='role_or_cutoff'))
            continue
        parsed, reasons = calls(turn['text'])
        if not parsed:
            exclusions.append(dict(identity, reason='no_literal_plan_call', detail=reasons))
            continue
        nearby = recall(hit['session_id'], f"{hit['seq']+1}..{hit['seq']+8}")
        proofs = []
        for t in nearby['turns']:
            if t['role'] == 'tool' and (COMPLETION.search(t['text']) or '\U0001f7e9 pixel plan ' in t['text'] or t['text'].strip() == 'No plan findings.'):
                proofs.append({'ref': ref, 'seq': t['seq'], 'text_sha256': digest(t['text']),
                               'turn_truncated': t['truncated'], 'evidence': 'adjacent plan output; not call-ID matched'})
        for index, call in enumerate(parsed):
            corroboration = corroborating_actions(call, turn['ts'], result['session']['cwd'], actions)
            record = dict(call, id=f"observed-{hit['session_id']}-{hit['seq']}-{index}",
                          source=identity, source_turn_sha256=digest(turn['text']),
                          timestamp_ms=turn['ts'], timestamp_source=hit['ts_source'],
                          cwd=result['session']['cwd'], historical_head=None,
                          source_session=result['session']['source_session_id'],
                          parent_session_id=result['session']['parent_session_id'],
                          is_subagent=result['session']['is_subagent'],
                          proofs=proofs, corroborating_action_ids=corroboration,
                          adjacent_window_capped=bool(nearby.get('truncated')),
                          verification='action_corroborated' if corroboration else ('adjacent_output' if proofs else 'invocation_only'))
            if call['prompt']:
                record['input_sha256'] = digest(call['prompt'])
            records.append(record)
    with args.output.open('x') as stream:
        json.dump({'records': records, 'exclusions': exclusions, 'search_hits': len(hits['hits']),
                   'cutoff_ms': CUTOFF_MS, 'historical_heads_proven': 0,
                   'privacy': 'local-only derived request records; do not publish'}, stream, indent=2)
        stream.write('\n')
    print('Extracted',len(records),'calls from',len(hits['hits']),'hits; excluded turns',len(exclusions))


if __name__ == '__main__':
    main()
