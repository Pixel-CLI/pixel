#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Corpus selection and annotation integrity tests; no quality predictions."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


def module(filename, name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


corpus = module('bench-plan-real-corpus.py', 'real_corpus')
evaluate = module('bench-plan-real-evaluate.py', 'real_evaluate')


class CorpusContract(unittest.TestCase):
    def fixtures(self, root, fork=False):
        for repo in corpus.REPOSITORIES:
            slug = repo.replace('/', '--')
            (root/(slug+'-pin.json')).write_text(json.dumps({'commit': 'a'*40}))
            (root/(slug+'-repository.json')).write_text(json.dumps({'fork': fork}))
            issues = [dict(number=n, title=f'Targeted behavior in {repo}', body='Preserve this exact request body.',
                           created_at=f'2026-01-0{n}T00:00:00Z', updated_at='2026-01-09T00:00:00Z',
                           html_url=f'https://github.com/{repo}/issues/{n}') for n in [1,2,3]]
            issues[2]['pull_request'] = {'url': 'https://example.com/pr'}
            (root/(slug+'-issues.json')).write_text(json.dumps(issues))

    def test_selection_uses_date_not_issue_keywords_and_preserves_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixtures(root)
            result = corpus.select(root, per_repo=1)
            self.assertEqual(len(result['requests']), 3)
            self.assertTrue(all(r['number'] == 2 for r in result['requests']))
            for row in result['requests']:
                self.assertEqual(row['text'], f"Targeted behavior in {row['repository']}\n\nPreserve this exact request body.")
                self.assertEqual(row['text_sha256'], hashlib.sha256(row['text'].encode()).hexdigest())
            self.assertEqual(sum(r['reason']=='pull-request' for r in result['exclusions']),3)

    def test_fork_and_insufficient_corpus_fail_instead_of_leaking_or_shrinking(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixtures(root, fork=True)
            with self.assertRaisesRegex(ValueError, 'fork'):
                corpus.select(root, per_repo=1)
            self.fixtures(root)
            with self.assertRaisesRegex(ValueError, 'insufficient'):
                corpus.select(root, per_repo=3)

    def test_duplicated_paginated_records_are_not_silently_replaced(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixtures(root)
            path = root/(corpus.REPOSITORIES[0].replace('/','--')+'-issues.json')
            path.with_name(path.stem+'-page2.json').write_text(path.read_text())
            with self.assertRaisesRegex(ValueError, 'changed during paginated'):
                corpus.select(root, per_repo=1)

    def gold_pair(self):
        request = dict(id='r',text='Locate the parser',text_sha256=hashlib.sha256(b'Locate the parser').hexdigest())
        gold = dict(id='r',labels=['by-concept'],evidence_quote='parser',rationale='targeted location',
                    family='repo:parser',ambiguous=False,unsupported_composition=False)
        return request, gold

    def test_gold_requires_literal_quote_and_unchanged_request(self):
        request, gold = self.gold_pair()
        self.assertEqual(evaluate.validate_gold([request],[gold])[0]['labels'],['by-concept'])
        with self.assertRaisesRegex(ValueError, 'nonliteral'):
            evaluate.validate_gold([request],[dict(gold,evidence_quote='invented')])
        with self.assertRaisesRegex(ValueError, 'changed'):
            evaluate.validate_gold([dict(request,text='Locate another parser')],[gold])

    def test_missing_unknown_or_non_boolean_annotations_fail(self):
        request, gold = self.gold_pair()
        for annotations in [[],[gold,gold],[dict(gold,id='other')],
                            [dict(gold,labels=['unknown'])],[dict(gold,ambiguous='false')]]:
            with self.assertRaises(ValueError):
                evaluate.validate_gold([request],annotations)


if __name__ == '__main__':
    unittest.main()
