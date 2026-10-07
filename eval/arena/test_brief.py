# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

import json
import unittest
from unittest.mock import patch

import brief as arena_brief


class BriefResolutionTest(unittest.TestCase):
    def setUp(self):
        arena_brief.ops.clear()
        arena_brief.brief.clear()
        arena_brief.brief.update({
            "anchors": [], "files": [], "symbols": [], "symbol_candidates": [],
            "callers": [], "exclusions": [], "unresolved": [], "ops": arena_brief.ops,
        })

    def test_path_anchor_selects_full_uid_before_impact(self):
        chosen_uid = "packages/ui/handleError.ts#handleError#function"
        symbols = {
            "symbols": [
                {"uid": "apps/site/handleError.ts#handleError#function", "name": "handleError",
                 "kind": "function", "path": "apps/site/handleError.ts", "start_line": 8},
                {"uid": chosen_uid, "name": "handleError", "kind": "function",
                 "path": "packages/ui/handleError.ts", "start_line": 4},
            ]
        }
        calls = []

        def fake_px(*args):
            calls.append(args)
            if args[0] == "search-content":
                return "packages/ui/handleError.ts\n"
            if args[0] == "find-symbol":
                return json.dumps(symbols)
            if args[0] == "impact":
                return json.dumps({"d1_will_break": []})
            self.fail(f"unexpected Pixel operation: {args}")

        with patch.object(arena_brief, "px", side_effect=fake_px):
            arena_brief.main("Rename handleError in packages/ui/handleError.ts and list callers")

        self.assertIn(("impact", chosen_uid, "--json"), calls)
        self.assertEqual(arena_brief.brief["symbols"][0]["path"], "packages/ui/handleError.ts")
        self.assertIn("path:line citations", arena_brief.brief["native_fallback"])

    def test_unresolved_ambiguity_keeps_impact_candidates(self):
        symbols = {"symbols": [
            {"uid": "a.ts#handleError#function", "name": "handleError", "path": "a.ts"},
            {"uid": "b.ts#handleError#function", "name": "handleError", "path": "b.ts"},
        ]}
        candidates = [{"uid": "a.ts#handleError#function", "name": "handleError",
                       "kind": "function", "path": "a.ts", "line": 3}]
        calls = []

        def fake_px(*args):
            calls.append(args)
            if args[0] == "search-content":
                return ""
            if args[0] == "find-code":
                return ""
            if args[0] == "find-symbol":
                return json.dumps(symbols)
            if args[0] == "impact":
                return json.dumps({"candidates": candidates, "d1_will_break": []})
            self.fail(f"unexpected Pixel operation: {args}")

        with patch.object(arena_brief, "px", side_effect=fake_px):
            arena_brief.main("Rename handleError and list callers")

        self.assertIn(("impact", "handleError", "--json"), calls)
        self.assertEqual(arena_brief.brief["symbol_candidates"][0]["uid"], candidates[0]["uid"])
        self.assertIn("path:line citations", arena_brief.brief["native_fallback"])


if __name__ == "__main__":
    unittest.main()
