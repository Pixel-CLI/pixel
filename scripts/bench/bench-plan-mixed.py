#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Experimental conservative PlanQuery jobs; not wired into Pixel defaults."""
import argparse
import hashlib
import json
import re
import socket
import sys

MAX_INPUT = 6000
MAX_JOBS = 8
LIMITS = {'dead-interactive': None, 'dead-code': None, 'hotspots': 10,
          'recent-changes': 20, 'by-concept': None}
VERBS = r'find|list|show|audit|identify|rank|review|inspect|locate|search|check|enumerate|scan|compare|investigate|prioritize|fix|implement|change|explain|trace'
SPLIT = re.compile(r'\n|;|\b(?:and|but|then)\s+(?=(?:please\s+)?(?:' + VERBS + r')\b)', re.I)
QUOTED = re.compile(r'```.*?(?:```|$)|~~~.*?(?:~~~|$)|`[^`\n]*`|"[^"\n]*"|(?<!\w)\x27[^\x27\n]*\x27(?!\w)', re.S)
NEGATION = re.compile(r"\b(?:do\s+not|don\x27t|never|without|ignore|skip|not)\b", re.I)


def masked(text):
    # Retain original offsets/newlines for auditable request evidence.
    return QUOTED.sub(lambda m: ''.join('\n' if c == '\n' else ' ' for c in m[0]), text)


def signals(text):
    found = []
    if re.search(r'\b(?:jsx|interactive|buttons?|links?|navlink|controls?)\b', text) and re.search(
            r'\b(?:handlerless|unwired|inert)\b|(?:missing|without|no)\s+(?:an?\s+|event\s+|action\s+)*handlers?', text):
        found.append('dead-interactive')
    if re.search(r'\b(?:zero[- ]caller|uncalled|unreferenced|unused|dead code|no callers)\b', text) and re.search(
            r'\b(?:functions?|methods?|callables?|routines?|helpers?|dead code)\b', text):
        found.append('dead-code')
    if re.search(r'\b(?:highest|high|most|rank|prioriti\w*|dependency|cross-file)\b', text) and re.search(
            r'fan[- ]in|distinct.{0,40}(?:calling|call|files)|files.{0,50}call into|depended[- ](?:on|upon)', text):
        found.append('hotspots')
    if re.search(r'\b(?:recent|latest|last 30 days|this month)\b', text) and re.search(
            r'\b(?:churn|history|commits?|changes)\b|changed files|files.{0,40}(?:touched|changed)', text):
        found.append('recent-changes')
    return found


def route(text):
    if not isinstance(text, str) or not text.strip():
        raise ValueError('request must be a nonempty string')
    if len(text) > MAX_INPUT:
        raise ValueError(f'request exceeds {MAX_INPUT} characters; not truncated')
    visible = masked(text)
    jobs, warnings = [], []
    start = 0
    spans = []
    for match in SPLIT.finditer(visible):
        spans.append((start, match.start()))
        start = match.end()
    spans.append((start, len(text)))
    for start, end in spans:
        raw = text[start:end]
        affirmative_raw = raw
        clean = visible[start:end].lower().strip(' \t,#*-')
        if not clean:
            continue
        negation = NEGATION.search(clean)
        # 'without a handler' describes a defect, not an excluded operation.
        if negation and not re.match(r'without\s+(?:an?\s+|event\s+|action\s+)*handlers?\b', clean[negation.start():]):
            clean = clean[:negation.start()].strip()
            raw_negation = NEGATION.search(visible[start:end])
            affirmative_raw = raw[:raw_negation.start()].rstrip()
            warnings.append('negated_or_excluded_suffix_not_used_for_scan_detection')
        if not clean:
            continue
        labels = signals(clean)
        scoped = re.search(r'\b(?:only|intersect\w*|restricted|limited|filter\w*)\b', clean)
        path_scope = re.search(r'\b(?:in|under|within)\s+\S*/\S*', clean)
        temporal_scope = re.search(r'\b(?:in|within|among)\s+(?:the\s+)?recent(?:ly)?\b', clean)
        if (scoped and len(labels) >= 2) or (labels and path_scope) or (temporal_scope and 'dead-code' in labels):
            return {'status': 'unsupported', 'jobs': [], 'warnings': ['relational_or_path_scoping_is_not_independent_union']}
        imperative = re.match(r'^(?:please\s+)?(?:' + VERBS + r')\b', clean)
        if not imperative:
            continue
        evidence = {'start': start, 'end': end, 'text': raw}
        if labels:
            for label in labels:
                jobs.append({'query': label, 'prompt': None, 'tag': None,
                             'limit': LIMITS[label], 'evidence': [evidence]})
        elif re.match(r'^(?:please\s+)?(?:locate|find|inspect|explain|trace|fix|implement|change|investigate)\b', clean):
            # Keep the clause's original literals: they may name the target.
            query = re.sub(r'^(?:please\s+)?(?:locate|find|inspect|explain|trace|fix|implement|change|investigate)\s+', '', affirmative_raw.strip(' \t,#*-'), flags=re.I)
            jobs.append({'query': 'by-concept', 'prompt': query, 'tag': None,
                         'limit': None, 'evidence': [evidence]})
    if not jobs:
        jobs = [{'query': 'by-concept', 'prompt': text, 'tag': None, 'limit': None,
                 'evidence': [{'start': 0, 'end': len(text), 'text': text}]}]
        warnings.append('no_supported_affirmative_scan_detected_concept_fallback')
    unique = {}
    for job in jobs:
        key = (job['query'], job['prompt'], job['tag'], job['limit'])
        if key in unique:
            unique[key]['evidence'].extend(job['evidence'])
        else:
            unique[key] = job
    if len(unique) > MAX_JOBS:
        raise ValueError(f'request exceeds {MAX_JOBS} jobs; no jobs dropped')
    return {'status': 'supported', 'jobs': list(unique.values()),
            'warnings': sorted(set(warnings)), 'input_sha256': hashlib.sha256(text.encode()).hexdigest(),
            'policy': 'experimental_independent_queries_not_a_semantic_proof',
            'caps': {'input_characters': MAX_INPUT, 'jobs': MAX_JOBS, 'hotspots': 10,
                     'recent_changes_files': 20, 'recent_changes_days': 30, 'concept_matches': 20}}


def request_for(job):
    label = job['query']
    if label not in LIMITS or job['limit'] != LIMITS[label] or job['tag'] is not None:
        raise ValueError('unknown query or changed fixed operation parameters')
    if label == 'by-concept':
        if not isinstance(job['prompt'], str) or not job['prompt'].strip() or len(job['prompt']) > MAX_INPUT:
            raise ValueError('invalid concept prompt')
    elif job['prompt'] is not None:
        raise ValueError('specialized query unexpectedly carries concept input')
    return {'op': 'plan', 'query': label, 'prompt': job['prompt'], 'tag': job['tag'], 'limit': job['limit']}


def rpc(path, request):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(60)
        sock.connect(path)
        sock.sendall((json.dumps(request) + '\n').encode())
        with sock.makefile('rb') as stream:
            raw = stream.readline(8 * 1024 * 1024)
    if not raw.endswith(b'\n'):
        raise ValueError('incomplete or capped daemon response')
    return json.loads(raw)


def execute(plan, transport):
    if plan['status'] != 'supported' or not 1 <= len(plan['jobs']) <= MAX_JOBS:
        raise ValueError('unsupported or invalid plan cannot execute')
    requests = [request_for(job) for job in plan['jobs']]
    results = []
    for request in requests:
        envelope = transport(request)
        if envelope.get('ok') is not True or envelope.get('error') is not None:
            raise RuntimeError(json.dumps({'failed_request': request, 'response': envelope, 'completed': results}))
        if envelope.get('result') is None or envelope.get('op') != 'plan':
            raise ValueError('invalid daemon Plan envelope')
        results.append({'request': request, 'response': envelope})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('request')
    parser.add_argument('--socket', help='opt-in execution against an existing daemon')
    args = parser.parse_args()
    plan = route(args.request)
    if args.socket:
        plan['results'] = execute(plan, lambda req: rpc(args.socket, req))
    json.dump(plan, sys.stdout, indent=2)
    print()


if __name__ == '__main__':
    main()
