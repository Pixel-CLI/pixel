#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract tests for scoring, leakage checks and name-set semantics."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('pilot', Path(__file__).with_name('bench-plan-routing.py'))
pilot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pilot)


class PilotContract(unittest.TestCase):
    def test_multilabel_cost_penalizes_missing_queries(self):
        rows = [dict(id='x', labels=['dead-code', 'hotspots'], candidate=['dead-code', 'recent-changes'])]
        m = pilot.metrics(rows, 'candidate')
        self.assertEqual(m['correct'], 0)
        self.assertEqual(m['weighted_cost'], 3)
        self.assertEqual(m['per_label']['hotspots']['fn'], 1)
        self.assertEqual(m['macro_f1'], 0.2)

    def test_perfect_and_wholly_wrong_predictions_differ(self):
        row = dict(id='x', labels=['by-concept'], candidate=['by-concept'])
        self.assertEqual(pilot.metrics([row], 'candidate')['exact_set'], 1)
        row['candidate'] = ['dead-code']
        self.assertEqual(pilot.metrics([row], 'candidate')['exact_set'], 0)

    def test_threshold_is_multilabel_with_exclusive_fallback(self):
        self.assertEqual(pilot.decode([0.5, 0.8, 0.1, 0.2]), ['dead-code', 'dead-interactive'])
        self.assertEqual(pilot.decode([0.49]*4), ['by-concept'])

    def test_invalid_probability_does_not_become_fallback(self):
        for p in [[float('nan')]*4, [float('inf')]*4, [0.1], [-0.1]*4, [1.1]*4]:
            with self.assertRaises(ValueError):
                pilot.decode(p)

    def test_baseline_keeps_existing_false_positive_and_whole_word_contract(self):
        self.assertEqual(pilot.baseline('Do not remove unused code'), ['dead-code'])
        self.assertEqual(pilot.baseline('unlinked invoices and debugging clicked handlers'), ['by-concept'])
        self.assertEqual(pilot.baseline('buttons and links'), ['dead-interactive'])
        self.assertEqual(pilot.baseline('unused helpers; refactor; recent bug'), ['dead-code', 'hotspots', 'recent-changes'])

    def test_normalized_duplicates_across_splits_rejected(self):
        row = dict(id='a', text='Find helpers!', labels=['by-concept'], family='one',
                   split='train', repository='pixel', commit='abc', evidence='semantic', provenance='synthetic')
        with self.assertRaisesRegex(ValueError, 'duplicate normalized'):
            pilot.validate([row, dict(row, id='b', family='two', split='test', text='find HELPERS')])

    def test_gold_may_require_concept_plus_specialized_query(self):
        row = dict(id='a', text='task', family='one', split='test', repository='pixel',
                   commit='abc', evidence='semantic', provenance='synthetic', labels=['by-concept', 'hotspots'])
        pilot.validate([row])
        row['candidate'] = ['hotspots']
        self.assertEqual(pilot.metrics([row], 'candidate')['weighted_cost'], 2)

    def test_unknown_and_empty_labels_rejected(self):
        row = dict(id='a', text='task', family='one', split='test', repository='pixel',
                   commit='abc', evidence='semantic', provenance='synthetic')
        for labels in [['unknown'], []]:
            with self.assertRaises(ValueError):
                pilot.validate([dict(row, labels=labels)])


if __name__ == '__main__':
    unittest.main()
