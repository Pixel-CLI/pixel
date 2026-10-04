#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Observable mixed-job contracts, separate from the locked quality corpus."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('mixed', Path(__file__).with_name('bench-plan-mixed.py'))
mixed = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mixed)


class MixedContract(unittest.TestCase):
    def test_mixed_jobs_keep_clause_input_and_order(self):
        text = 'Locate classify_prompt; rank files with highest fan-in'
        plan = mixed.route(text)
        self.assertEqual([j['query'] for j in plan['jobs']], ['by-concept', 'hotspots'])
        self.assertEqual(plan['jobs'][0]['prompt'], 'classify_prompt')
        self.assertEqual(plan['jobs'][1]['limit'], 10)
        for job in plan['jobs']:
            for span in job['evidence']:
                self.assertEqual(text[span['start']:span['end']], span['text'])

    def test_concept_locations_do_not_collapse(self):
        plan = mixed.route('locate parser; locate serializer')
        self.assertEqual([j['prompt'] for j in plan['jobs']], ['parser', 'serializer'])

    def test_independent_scans_are_multilabel(self):
        plan = mixed.route('Find uncalled methods; review recent churn')
        self.assertEqual([j['query'] for j in plan['jobs']], ['dead-code', 'recent-changes'])
        self.assertEqual(plan['jobs'][1]['limit'], 20)

    def test_defect_without_handler_is_not_a_negation(self):
        self.assertEqual(mixed.route('Find JSX buttons without an event handler')['jobs'][0]['query'], 'dead-interactive')

    def test_clause_leading_negation_is_not_a_scan(self):
        plan = mixed.route('Do not audit unused functions; locate parser')
        self.assertEqual([j['query'] for j in plan['jobs']], ['by-concept'])
        self.assertEqual(plan['jobs'][0]['prompt'], 'parser')
        self.assertTrue(plan['warnings'])

    def test_negated_suffix_not_part_of_concept_input(self):
        plan = mixed.route('Locate parser without reviewing recent history')
        self.assertEqual(plan['jobs'][0]['prompt'], 'parser')

    def test_quoted_targets_remain_available_to_concept(self):
        for text in ['Find the parser for "unused functions"', 'Locate `dead code` documentation']:
            plan = mixed.route(text)
            self.assertEqual([j['query'] for j in plan['jobs']], ['by-concept'])
            self.assertIn('"unused functions"' if 'unused' in text else '`dead code`', plan['jobs'][0]['prompt'])

    def test_fenced_examples_do_not_request_scans(self):
        for suffix in ['\n```', '']:
            plan = mixed.route('Fix cache\n```text\nlist unused functions' + suffix)
            self.assertEqual([j['query'] for j in plan['jobs']], ['by-concept'])

    def test_local_bug_refactor_and_remove_do_not_request_global_scan(self):
        for text in ['Fix the button color bug', 'Refactor parser error handling', 'Remove expired cache entries']:
            self.assertEqual([j['query'] for j in mixed.route(text)['jobs']], ['by-concept'])

    def test_relational_intersection_is_rejected_before_execution(self):
        for text in ['Find unused functions only in recently changed files', 'Find unused functions in recent changes']:
            plan = mixed.route(text)
            self.assertEqual(plan['status'], 'unsupported')
            self.assertEqual(plan['jobs'], [])
            with self.assertRaises(ValueError):
                mixed.execute(plan, lambda _: self.fail('unsupported plan reached transport'))

    def test_path_scoped_global_scan_is_not_misrepresented(self):
        self.assertEqual(mixed.route('Find unused functions under src/parser')['status'], 'unsupported')

    def test_caps_refuse_instead_of_truncating(self):
        for text in ['', None, 'x' * 6001, ';'.join('locate symbol'+str(i) for i in range(9))]:
            with self.assertRaises(ValueError):
                mixed.route(text)

    def test_repeated_query_keeps_all_evidence(self):
        plan = mixed.route('Find uncalled methods; list unused functions')
        self.assertEqual(len(plan['jobs']), 1)
        self.assertEqual(len(plan['jobs'][0]['evidence']), 2)

    def test_full_envelopes_and_duplicate_findings_are_retained_per_job(self):
        plan = mixed.route('Locate parser; rank highest fan-in files')
        calls = []
        shared = {'file': 'parser.rs', 'line': 1, 'label': 'same evidence'}
        def transport(request):
            calls.append(request)
            return {'ok': True, 'op': 'plan', 'error': None,
                    'result': {'queries': [request['query']], 'findings': [shared]},
                    'epistemics': {'closed_world': False}, 'warnings': ['capped'], 'snapshot': {'head': 'abc'}}
        results = mixed.execute(plan, transport)
        self.assertEqual(len(results), 2)
        self.assertEqual(calls[0]['prompt'], 'parser')
        self.assertIsNone(calls[0]['limit'])
        self.assertEqual(calls[1]['limit'], 10)
        for r in results:
            self.assertEqual(r['response']['result']['findings'], [shared])
            self.assertEqual(r['response']['warnings'], ['capped'])
            self.assertFalse(r['response']['epistemics']['closed_world'])
            self.assertEqual(r['response']['snapshot']['head'], 'abc')

    def test_error_does_not_become_empty_success(self):
        plan = mixed.route('Locate parser')
        with self.assertRaisesRegex(RuntimeError, 'broken graph'):
            mixed.execute(plan, lambda _: {'ok': False, 'error': {'message': 'broken graph'}})

    def test_tampered_parameters_rejected_before_transport(self):
        plan = mixed.route('Rank highest fan-in files')
        plan['jobs'][0]['limit'] = 500
        with self.assertRaises(ValueError):
            mixed.execute(plan, lambda _: self.fail('tampered plan reached transport'))


if __name__ == '__main__':
    unittest.main()
