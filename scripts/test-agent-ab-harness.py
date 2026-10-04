#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of the agent A/B harness (eval/run.sh, score.py, report.py), offline.

No model is called. The whole pipeline runs against fixture CLIs
(eval/fixtures/fake-claude, fake-codex, fake-pixel) in a disposable git
repository with a disposable HOME: arm build, counterbalanced dispatch over
three repetitions and two hosts, held-out verifier execution, scoring and
the host × task-class report. The fixture agent behaves well exactly when
`pixel` runs on its PATH, so every assertion on a score is an assertion on
arm isolation.

It also checks the real scenario corpus (eval/scenarios, eval/heldout) for
schema defects and answer leaks, the arm-order port against the vectors
eval/controlled.ts computes, and the transcript metrics on hand-written
commands.
"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
EVAL = ROOT / "eval"
sys.path.insert(0, str(EVAL))
sys.path.insert(0, str(EVAL / "lib"))

import arm_order  # noqa: E402
import filter_hooks  # noqa: E402
import scenario  # noqa: E402
import score  # noqa: E402

ARMS = ["baseline", "quiet", "full"]


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else "absent"


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                   env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                        "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"})


def pixel_hook(verb, matcher=None):
    group = {"hooks": [{"type": "command", "command": f"'/opt/fake/pixel' run-hook {verb} --provider claude", "timeout": 10}]}
    if matcher:
        group["matcher"] = matcher
    return group


class UnitContracts(unittest.TestCase):
    def test_arm_order_matches_the_controlled_runner(self):
        # bun: armOrder(seed, rep, case) from eval/controlled.ts, arms
        # ["retrieval","gates","gates_classifier"] -> same indices here.
        names = ["retrieval", "gates", "gates_classifier"]
        vectors = [
            ("ab626", 0, "claude/ab-x", ["retrieval", "gates_classifier", "gates"]),
            ("ab626", 1, "claude/ab-x", ["gates_classifier", "gates", "retrieval"]),
            ("ab626", 2, "claude/ab-x", ["gates", "retrieval", "gates_classifier"]),
            ("ab626", 3, "codex/ab-y", ["gates", "retrieval", "gates_classifier"]),
            ("offline-example", 0, "pi/fixture", ["retrieval", "gates_classifier", "gates"]),
        ]
        for seed, rep, case, expected in vectors:
            self.assertEqual(arm_order.arm_order(seed, rep, case, names), expected, (seed, rep, case))

    def test_every_arm_holds_every_position_once_per_rotation(self):
        for case in ("claude/a", "codex/b", "claude/c"):
            orders = [arm_order.arm_order("seed", rep, case, ARMS) for rep in range(3)]
            for position in range(3):
                self.assertEqual(sorted(o[position] for o in orders), sorted(ARMS), (case, orders))

    def test_filter_hooks_keeps_only_the_session_start_doctrine_when_quiet(self):
        doc = {"hooks": {
            "SessionStart": [pixel_hook("session-start"), pixel_hook("post-compaction", "compact"),
                             {"hooks": [{"type": "command", "command": "'/opt/fake/pixel' run-hook task-event --event session-start"}]}],
            "UserPromptSubmit": [pixel_hook("prompt-submit")],
            "PostToolUse": [pixel_hook("metrics", "Bash"),
                            {"matcher": "Bash", "hooks": [{"type": "command", "command": "other-tool notify"}]}],
        }, "theme": "dark"}
        def commands(d):
            return sorted(h["command"] for gs in d.get("hooks", {}).values() for g in gs for h in g["hooks"])

        quiet = filter_hooks.filter_doc(doc, "quiet")
        self.assertEqual(commands(quiet), [
            "'/opt/fake/pixel' run-hook post-compaction --provider claude",
            "'/opt/fake/pixel' run-hook session-start --provider claude",
            "other-tool notify",
        ])
        self.assertEqual(quiet["theme"], "dark")
        self.assertEqual(commands(filter_hooks.filter_doc(doc, "none")), ["other-tool notify"])
        self.assertEqual(filter_hooks.filter_doc(doc, "full"), doc)

    def test_command_classification(self):
        cases = {
            "pixel search-content -F foo .": {"pixel"},
            "rtk pixel find-code 'output cap'": {"pixel"},
            "cd crates && /home/u/.local/bin/pixel who-calls x": {"pixel"},
            "bash -lc 'rg -n GRAPH_DB_FILE crates'": {"search"},
            "git grep -n foo": {"search"},
            "cargo test 2>&1 | grep FAILED": set(),
            "find . -name '*.rs' | xargs grep -l foo": {"find", "search"},
            "'/opt/pixel' run-hook session-start": set(),
            "timeout 30 grep -rn foo src": {"search"},
        }
        for command, kinds in cases.items():
            self.assertEqual(score.classify_command(command), kinds, command)

    def test_read_widths_follow_only_pixel_hits(self):
        calls = [
            {"kind": "bash", "command": "cat README.md", "output": ""},  # before any hit: not counted
            {"kind": "bash", "command": "pixel search-content -F x", "output": "crates/a/src/lib.rs:12: x\n./b.py:3: x"},
            {"kind": "bash", "command": "bash -lc \"sed -n '1,40p' crates/a/src/lib.rs\"", "output": ""},
            {"kind": "read", "path": "/tmp/wt/b.py", "limit": None, "output": ""},
            {"kind": "read", "path": "/tmp/wt/other.rs", "limit": 5, "output": ""},  # not a hit
            {"kind": "grep"},
        ]
        m = score.tool_metrics(calls)
        self.assertEqual((m["pixel_calls"], m["native_search_calls"]), (1, 1))
        self.assertEqual(m["pixel_hit_read_widths"], [40, None])
        self.assertEqual((m["pixel_hit_full_reads"], m["pixel_hit_read_median_lines"]), (1, 40))

    def test_the_real_corpus_is_sound(self):
        shallow = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--is-shallow-repository"],
                                 capture_output=True, text=True).stdout.strip() == "true"
        argv = [sys.executable, str(EVAL / "lib/scenario.py"), "check", str(EVAL / "scenarios"), str(EVAL / "heldout")]
        if not shallow:
            argv.append(str(ROOT))   # also: every pinned commit exists and holds no answer
        result = subprocess.run(argv, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        suite = subprocess.run([sys.executable, str(EVAL / "lib/scenario.py"), "list", str(EVAL / "scenarios"), "ab626"],
                               capture_output=True, text=True, check=True).stdout.split()
        classes = {json.loads((EVAL / "scenarios" / f"{s}.json").read_text())["task_class"] for s in suite}
        self.assertEqual(len(suite), 14, suite)
        self.assertEqual(classes, set(scenario.TASK_CLASSES), "every task class has a scenario")

    def test_every_answer_rubric_gives_its_reference_answer_full_marks(self):
        # heldout/<id>/reference-answer.md was checked against the pinned
        # tree; a rubric that cannot award it full marks, or that penalises
        # it, is a rubric defect, not a model failure.
        answers = 0
        for path in sorted((EVAL / "scenarios").glob("*.json")):
            s = json.loads(path.read_text())
            if s.get("suite") != "ab626" or s.get("mode", "answer") != "answer":
                continue
            answers += 1
            reference = (EVAL / "heldout" / s["id"] / "reference-answer.md").read_text()
            earned, _, penalties = score.score_answer(reference, s)
            self.assertEqual((earned, penalties), (sum(m["points"] for m in s["must"]), 0), s["id"])
            self.assertEqual(score.score_answer("I could not find it.", s)[0], 0, s["id"])
        self.assertEqual(answers, 9)


class OfflinePipeline(unittest.TestCase):
    """run.sh end to end: 3 scenarios × 3 arms × 3 reps × 2 hosts, fixture CLIs."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="pixel-ab-harness-")
        root = Path(cls.tmp.name).resolve()
        cls.root = root
        cls.real_prompt = Path.home() / ".local/share/pixel/agent-prompt.md"
        cls.real_prompt_sha = sha(cls.real_prompt)
        home = root / "home"
        cls.home = home
        (home / ".claude/skills/pixel-retrieval").mkdir(parents=True)
        (home / ".claude/skills/other-skill").mkdir(parents=True)
        (home / ".claude/CLAUDE.md").write_text("operator notes\n")
        (home / ".claude/.credentials.json").write_text('{"fixture": "login"}')
        (home / ".claude/settings.json").write_text(json.dumps({"hooks": {
            "SessionStart": [pixel_hook("session-start"), pixel_hook("post-compaction", "compact")],
            "UserPromptSubmit": [pixel_hook("prompt-submit")],
            "PostToolUse": [pixel_hook("metrics", "Bash"), pixel_hook("post-tool-use", "Edit")],
            "PreToolUse": [{"hooks": [{"type": "command", "command": "'/opt/fake/pixel' run-hook task-event --event pre-tool-use"}]}],
        }}))
        (home / ".codex").mkdir()
        (home / ".codex/config.toml").write_text(
            "model = \"x\"\ndeveloper_instructions = '''\n<!-- pixel:managed:begin -->\nUse pixel.\n<!-- pixel:managed:end -->\n'''\n")
        (home / ".codex/hooks.json").write_text(json.dumps({"hooks": {
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/opt/fake/pixel' run-hook prompt-submit --provider codex"}]}],
            "PostToolUse": [{"hooks": [{"type": "command", "command": "'/opt/fake/pixel' run-hook metrics --provider codex"}]}]}}))
        (home / ".codex/auth.json").write_text("{}")
        prompt = home / ".local/share/pixel/agent-prompt.md"
        prompt.parent.mkdir(parents=True)
        prompt.write_text("# installed pixel prompt\n")
        cls.deployed_sha = sha(prompt)
        cls.deployed = prompt

        bin_dir = root / "bin"
        bin_dir.mkdir()
        for name in ("fake-claude", "fake-codex", "fake-pixel"):
            shutil.copy(EVAL / "fixtures" / name, bin_dir / name)
        os.symlink(bin_dir / "fake-pixel", bin_dir / "pixel")

        repo = root / "repo"
        (repo / "src").mkdir(parents=True)
        (repo / "src/lib.rs").write_text("// fixture\n// BUG here\npub const NEEDLE: u32 = 1;\n")
        (repo / "AGENTS.md").write_text("# rules\n<!-- pixel:warp-retrieval:begin -->\nUse pixel.\n<!-- pixel:warp-retrieval:end -->\n")
        git("init", "-q", "-b", "main", cwd=repo)
        git("add", ".", cwd=repo)
        git("commit", "-qm", "pinned tree", cwd=repo)
        pinned = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True, check=True).stdout.strip()
        shutil.copytree(EVAL, repo / "eval", ignore=shutil.ignore_patterns("results", "arena-results", "scenarios", "heldout", "__pycache__"))
        scenarios = repo / "eval/scenarios"
        scenarios.mkdir()
        common = {"suite": "fixture", "commit": pinned, "max_turns": 7}
        fixtures = [
            {"id": "fx-answer", "task_class": "exact-identifier", "mode": "answer",
             "prompt": "[answer] Where is NEEDLE defined?",
             "must": [{"pattern": r"src/lib\.rs", "points": 2}, {"pattern": r"\bNEEDLE\b", "points": 1},
                      {"pattern": r"line 3|:3\b", "points": 1}], "never": []},
            {"id": "fx-edit", "task_class": "bugfix", "mode": "edit", "warm": "touch .warm-marker",
             "prompt": "[edit] Fix the BUG marker in src/lib.rs.",
             "verifier": {"script": "check.sh", "timeout_s": 60}, "must": [], "never": []},
            {"id": "fx-nousage", "task_class": "git-ops", "mode": "answer",
             "prompt": "[nousage] Which file holds NEEDLE?",
             "must": [{"pattern": r"src/lib\.rs", "points": 1}], "never": []},
        ]
        for s in fixtures:
            (scenarios / f"{s['id']}.json").write_text(json.dumps({**common, **s}))
        (repo / "eval/heldout/fx-edit").mkdir(parents=True)
        (repo / "eval/heldout/fx-edit/check.sh").write_text(
            "set -u\ntest -f \"$HELDOUT/check.sh\"\ngrep -q FIXED src/lib.rs && ! grep -q BUG src/lib.rs\n")
        git("add", ".", cwd=repo)
        git("commit", "-qm", "harness", cwd=repo)
        cls.repo, cls.pinned = repo, pinned
        cls.exclude = repo / ".git/info/exclude"
        cls.exclude_sha = sha(cls.exclude)

        cls.results = root / "results"
        cls.fake_log = root / "fake.jsonl"
        cls.pixel_log = root / "pixel.jsonl"
        cls.env = {
            **os.environ,
            "HOME": str(home), "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "CLIS": "claude codex", "ARMS": " ".join(ARMS), "SUITE": "fixture", "REPS": "3",
            "RESULTS": str(cls.results), "SCRATCH": str(root / "scratch"),
            "CLAUDE_BIN": str(bin_dir / "fake-claude"), "CODEX_BIN": str(bin_dir / "fake-codex"),
            "PIXEL_BIN": str(bin_dir / "fake-pixel"), "CLAUDE_MODEL": "claude-test-model",
            "CODEX_MODEL": "codex-test-model", "CODEX_REASONING": "low", "RUN_TIMEOUT": "120",
            "FAKE_LOG": str(cls.fake_log), "FAKE_PIXEL_LOG": str(cls.pixel_log),
        }
        for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "SCENARIOS", "ORDER_SEED", "MAX_TURNS"):
            cls.env.pop(key, None)
        cls.campaign = subprocess.run(["bash", str(repo / "eval/run.sh")], cwd=repo, env=cls.env,
                                 capture_output=True, text=True, timeout=900)
        cls.rerun = subprocess.run(["bash", str(repo / "eval/run.sh")], cwd=repo, env=cls.env,
                                   capture_output=True, text=True, timeout=900)
        cls.report = subprocess.run([sys.executable, str(repo / "eval/report.py"), "--results", str(cls.results),
                                     "--json", str(root / "report.json")], capture_output=True, text=True)
        # A legacy variant arm swaps the machine-global prompt; the quiet arm
        # that follows it in the rotation must see the installed one again.
        cls.swap_log = root / "fake-swap.jsonl"
        cls.swap = subprocess.run(["bash", str(repo / "eval/run.sh")], cwd=repo, capture_output=True, text=True,
                                  timeout=900, env={**cls.env, "ARMS": "vslim quiet baseline", "CLIS": "claude",
                                                    "SCENARIOS": "fx-answer", "RESULTS": str(root / "results-swap"),
                                                    "SCRATCH": str(root / "scratch-swap"), "FAKE_LOG": str(cls.swap_log)})
        # The documented command passes RESULTS and SCRATCH relative to the
        # operator's directory, while every run changes into its scenario
        # worktree: the paths must be resolved before that.
        cls.relative = subprocess.run(["bash", str(repo / "eval/run.sh")], cwd=root, capture_output=True, text=True,
                                      timeout=900, env={**cls.env, "ARMS": "baseline", "CLIS": "claude", "REPS": "1",
                                                        "SCENARIOS": "fx-answer", "RESULTS": "results-rel",
                                                        "SCRATCH": "scratch-rel",
                                                        "FAKE_LOG": str(root / "fake-rel.jsonl")})
        cls.relative_root = root
        cls.log = [json.loads(line) for line in cls.fake_log.read_text().splitlines()] if cls.fake_log.exists() else []
        cls.rows = json.loads((cls.results / "scores.json").read_text()) if (cls.results / "scores.json").exists() else []

    @classmethod
    def tearDownClass(cls):
        subprocess.run(["git", "worktree", "prune"], cwd=cls.repo, capture_output=True)
        cls.tmp.cleanup()

    def test_the_campaign_completes_and_a_rerun_skips_every_cell(self):
        self.assertEqual(self.campaign.returncode, 0, self.campaign.stdout[-4000:] + self.campaign.stderr[-4000:])
        self.assertEqual(self.rerun.returncode, 0, self.rerun.stderr[-4000:])
        self.assertEqual(self.rerun.stdout.count("=== skip "), 54, self.rerun.stdout[-2000:])
        self.assertEqual(len(self.log), 54, "the rerun must not call a host again")

    def test_relative_results_and_scratch_paths_resolve_before_the_runs(self):
        self.assertEqual(self.relative.returncode, 0, self.relative.stdout[-3000:] + self.relative.stderr[-3000:])
        self.assertNotIn("run failed", self.relative.stdout)
        rows = json.loads((self.relative_root / "results-rel/scores.json").read_text())
        self.assertEqual([(r["scenario"], r["arm"]) for r in rows], [("fx-answer", "baseline")])

    def test_a_host_that_never_reaches_the_model_stops_the_campaign_unrecorded(self):
        logged_out = self.relative_root / "home-logged-out"
        if not logged_out.exists():
            shutil.copytree(self.home, logged_out, symlinks=True)
            (logged_out / ".claude/.credentials.json").unlink()
        results = self.relative_root / "results-logged-out"
        run = subprocess.run(["bash", str(self.repo / "eval/run.sh")], cwd=self.repo, capture_output=True,
                             text=True, timeout=600,
                             env={**self.env, "HOME": str(logged_out), "CLIS": "claude", "ARMS": "baseline full",
                                  "REPS": "1", "SCENARIOS": "fx-answer", "RESULTS": str(results),
                                  "SCRATCH": str(self.relative_root / "scratch-logged-out"),
                                  "FAKE_LOG": str(self.relative_root / "fake-logged-out.jsonl")})
        self.assertEqual(run.returncode, 3, run.stdout[-2000:] + run.stderr[-2000:])
        self.assertIn("never reached the model", run.stderr)
        self.assertEqual(run.stdout.count("=== run"), 1, "the campaign stops at the first such cell")
        self.assertEqual(list(results.glob("rep-1/*.run.json")), [], "the cell is not recorded")
        self.assertFalse((results / "scores.json").exists())

    def test_a_missing_host_binary_refuses_the_campaign_before_any_cell(self):
        missing = subprocess.run(["bash", str(self.repo / "eval/run.sh")], cwd=self.repo, capture_output=True,
                                 text=True, timeout=120,
                                 env={**self.env, "CLIS": "claude", "CLAUDE_BIN": "/nonexistent/claude",
                                      "RESULTS": str(self.relative_root / "results-missing"),
                                      "SCRATCH": str(self.relative_root / "scratch-missing")})
        self.assertEqual(missing.returncode, 2, missing.stdout[-2000:] + missing.stderr[-2000:])
        self.assertIn("no claude executable at /nonexistent/claude", missing.stderr)
        self.assertNotIn("=== run", missing.stdout)

    def test_a_variant_payload_never_leaks_into_the_quiet_arm(self):
        self.assertEqual(self.swap.returncode, 0, self.swap.stdout[-3000:] + self.swap.stderr[-3000:])
        seen = [json.loads(line) for line in self.swap_log.read_text().splitlines()]
        slim = (EVAL / "variants/slim/agent-prompt.md").read_text().splitlines()[0]
        arms = [e["arm"] for e in seen]
        self.assertIn("quiet", arms[arms.index("vslim"):], f"no quiet run follows a variant: {arms}")
        self.assertEqual({e["deployed_prompt"] for e in seen if e["arm"] == "vslim"}, {slim})
        self.assertEqual({e["deployed_prompt"] for e in seen if e["arm"] == "quiet"}, {"# installed pixel prompt"})
        self.assertEqual(sha(self.deployed), self.deployed_sha, "the campaign left the variant deployed")

    def test_machine_state_is_left_as_found(self):
        self.assertEqual(sha(self.deployed), self.deployed_sha, "the deployed prompt was not restored")
        self.assertEqual(sha(self.exclude), self.exclude_sha, "info/exclude was not restored")
        self.assertEqual(sha(self.real_prompt), self.real_prompt_sha, "the operator's real deployed prompt changed")
        self.assertFalse((self.home / ".local/share/pixel/.eval-prompt.lock").exists())

    def test_hosts_receive_the_pinned_model_and_flags(self):
        claude = [e for e in self.log if e["host"] == "claude"]
        codex = [e for e in self.log if e["host"] == "codex"]
        self.assertEqual((len(claude), len(codex)), (27, 27))
        self.assertEqual({e["model"] for e in claude}, {"claude-test-model"})
        self.assertEqual({e["max_turns"] for e in claude}, {"7"})
        self.assertEqual({e["model"] for e in codex}, {"codex-test-model"})
        self.assertEqual({tuple(e["config_overrides"]) for e in codex}, {('model_reasoning_effort="low"',)})
        self.assertTrue(all(e["hook_trust_bypass"] for e in codex))

    def test_arms_are_isolated(self):
        by_arm = {}
        for e in self.log:
            by_arm.setdefault((e["host"], e["arm"]), []).append(e)
        for host in ("claude", "codex"):
            base, quiet, full = (by_arm[(host, a)] for a in ARMS)
            self.assertTrue(all(not e["pixel_ok"] and not e["dot_pixel"] for e in base), "baseline saw pixel")
            self.assertTrue(all(e["pixel_ok"] and e["dot_pixel"] for e in quiet + full))
            self.assertFalse(any(e["eval_visible"] for e in base + quiet + full), "the answers were visible")
            self.assertEqual({tuple(e["hooks"]) for e in base}, {()})
        self.assertEqual({tuple(e["hooks"]) for e in by_arm[("claude", "quiet")]}, {("post-compaction", "session-start")})
        self.assertEqual({tuple(e["hooks"]) for e in by_arm[("claude", "full")]},
                         {("guard", "metrics", "post-compaction", "post-tool-use", "prompt-submit", "session-start", "task-event")})
        self.assertEqual({tuple(e["hooks"]) for e in by_arm[("codex", "quiet")]}, {("session-start",)})
        self.assertEqual({tuple(e["hooks"]) for e in by_arm[("codex", "full")]},
                         {("composed-guard", "metrics", "prompt-submit", "session-start")})
        self.assertEqual({e["managed_block"] for e in by_arm[("codex", "baseline")]}, {False})
        self.assertEqual({e["managed_block"] for e in by_arm[("codex", "full")]}, {True})
        self.assertEqual({tuple(e["skills"]) for e in by_arm[("claude", "baseline")]}, {("other-skill",)})
        self.assertEqual({tuple(e["skills"]) for e in by_arm[("claude", "quiet")]}, {("other-skill", "pixel-retrieval")})
        warm = [e for e in self.log if e["host"] == "claude" and e["prompt"].startswith("[edit]")]
        self.assertTrue(warm and all(e["warm_marker"] for e in warm), "the warm step did not run before the agent")
        self.assertEqual(len({e["cargo_target"] for e in warm}), 3, "one build cache per arm")

    def test_arm_order_rotates_across_repetitions(self):
        runs = [json.loads(p.read_text()) for p in self.results.rglob("*.run.json")]
        self.assertEqual(len(runs), 54)
        cells = {}
        for r in runs:
            cells.setdefault((r["cli"], r["scenario"]), {}).setdefault(r["position"], set()).add(r["arm"])
        for cell, positions in cells.items():
            self.assertEqual(sorted(positions), [1, 2, 3], cell)
            for position, arms in positions.items():
                self.assertEqual(arms, set(ARMS), (cell, position))
        r = runs[0]
        for key in ("model", "cli_version", "commit", "order_seed", "wall_ms", "exit_code", "pixel_version"):
            self.assertIn(key, r)
        self.assertEqual({r["commit"] for r in runs}, {self.pinned})
        self.assertEqual({r["cli_version"] for r in runs}, {"9.9.9 (Fake Claude)", "codex-cli 0.0.0-fake"})

    def test_the_heldout_verifier_judges_edit_runs(self):
        edits = [r for r in self.rows if r["scenario"] == "fx-edit"]
        self.assertEqual(len(edits), 18)
        for r in edits:
            self.assertEqual(r["verifier_passed"], r["arm"] != "baseline", r)
            self.assertEqual(r["quality"], 1.0 if r["arm"] != "baseline" else 0.0)

    def test_scores_carry_metrics_and_unknown_usage_stays_unknown(self):
        self.assertEqual(len(self.rows), 54)
        answer = {(r["cli"], r["arm"]): r for r in self.rows if r["scenario"] == "fx-answer" and r["rep"] == 1}
        self.assertEqual(answer[("claude", "full")]["score"], 4)
        self.assertEqual(answer[("claude", "baseline")]["score"], 3)
        self.assertEqual(answer[("claude", "full")]["pixel_calls"], 1)
        self.assertEqual(answer[("claude", "full")]["pixel_hit_read_widths"], [20])
        self.assertEqual(answer[("claude", "baseline")]["native_search_calls"], 1)
        self.assertEqual(answer[("claude", "baseline")]["pixel_calls"], 0)
        self.assertEqual(answer[("claude", "full")]["total_input_tokens"], 1150)
        self.assertEqual(answer[("claude", "full")]["cost_usd"], 0.01)
        self.assertEqual(answer[("codex", "full")]["pixel_hit_read_widths"], [20])
        self.assertEqual(answer[("codex", "baseline")]["native_search_calls"], 1)
        self.assertEqual(answer[("codex", "full")]["input_tokens"], 2000)
        self.assertIsNone(answer[("codex", "full")]["cost_usd"])
        nousage = [r for r in self.rows if r["scenario"] == "fx-nousage" and r["cli"] == "codex"]
        self.assertTrue(nousage and all(r["input_tokens"] is None and r["gen_tokens"] is None for r in nousage))
        self.assertTrue(all(isinstance(r["wall_ms"], int) for r in self.rows))

    def test_the_report_is_per_host_and_task_class(self):
        self.assertEqual(self.report.returncode, 0, self.report.stderr)
        out = self.report.stdout
        print("\n" + out, file=sys.stderr)   # the table a reader of the log can check by eye
        self.assertIn("### claude", out)
        self.assertIn("### codex", out)
        report = json.loads((self.root / "report.json").read_text())
        for host in ("claude", "codex"):
            v = report["verdicts"][host]
            self.assertEqual(v["bugfix"]["full"]["verdict"], "win")
            self.assertEqual((v["bugfix"]["full"]["wins"], v["bugfix"]["full"]["n"]), (3, 3))
            self.assertEqual(v["exact-identifier"]["quiet"]["verdict"], "win")
            self.assertEqual(v["git-ops"]["full"]["verdict"], "tie")
            self.assertEqual(v["all"]["full"]["n"], 9)
            a = report["adoption"][host]
            self.assertEqual(a["baseline"]["pixel_calls"], 0)
            self.assertEqual(a["full"]["runs_using_pixel"], 9)
        self.assertEqual(report["verdicts"]["codex"]["git-ops"]["full"]["token_ratio"], [None, 0],
                         "missing usage must not become a ratio")


if __name__ == "__main__":
    unittest.main(verbosity=2)
