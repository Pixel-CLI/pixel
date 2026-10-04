#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of .cargo/mutants.toml: an exclusion only ever covers a bench or
a crate-root build script.

`exclude_globs` is the one place where a file can leave the mutation gate
without anyone noticing: cargo-mutants reports nothing for a path it was
told to skip, so the `Mutants` job stays green over code it never mutated.
The trap this file exists for: `**/build.rs` reads as "cargo build scripts"
but matches any source file of that name, and this workspace has one --
`crates/pixel-graph/src/build.rs`, the 2000-line module behind `build_graph`,
`tree_delta` and the freshness signature. It sat outside the gate for as
long as the glob did.

So the rule below is deliberately narrow: every Rust file an exclusion
covers must be a bench (not shipped, no tests of its own) or a build script
at a crate root (a separate compilation unit; a mutated one changes what the
build reports, not what a test asserts). Anything else is a module, and a
module belongs to the gate.
"""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / ".cargo/mutants.toml"
GATE = REPO / "scripts/mutants-gate.py"

#: A bench: the dedicated crate, or a `benches/` directory in any crate.
BENCH = re.compile(r"^crates/pixel-bench/|(^|/)benches/")
#: A cargo build script: `build.rs` directly at a crate root, never deeper.
BUILD_SCRIPT = re.compile(r"^crates/[^/]+/build\.rs$")


def exclude_globs() -> list[str]:
    """The `exclude_globs` array, read without a TOML dependency."""
    text = CONFIG.read_text()
    match = re.search(r"^exclude_globs\s*=\s*\[(.*?)\]", text, re.M | re.S)
    assert match, f"{CONFIG} declares no exclude_globs"
    return re.findall(r'"([^"]+)"', match.group(1))


def to_regex(glob: str) -> re.Pattern[str]:
    """Translate one glob to a regex with globset's semantics.

    The distinction this contract turns on: `*` stops at a path separator,
    `**` crosses them. Anything else is matched literally.
    """
    out = ["^"]
    i = 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    out.append("$")
    return re.compile("".join(out))


def rust_files() -> list[str]:
    """Every tracked-looking Rust file, repo-relative, excluding build output."""
    skip = {"target", ".git", ".pixel", "node_modules"}
    found = []
    for path in REPO.rglob("*.rs"):
        rel = path.relative_to(REPO)
        if skip.isdisjoint(rel.parts):
            found.append(rel.as_posix())
    return sorted(found)


class MutantsConfigContract(unittest.TestCase):
    def test_every_excluded_rust_file_is_a_bench_or_a_crate_root_build_script(self):
        patterns = [to_regex(g) for g in exclude_globs()]
        files = rust_files()
        self.assertTrue(files, "found no Rust files to check")
        excluded = [f for f in files if any(p.match(f) for p in patterns)]
        offenders = [
            f for f in excluded if not BENCH.search(f) and not BUILD_SCRIPT.match(f)
        ]
        self.assertEqual(
            offenders,
            [],
            "these files are excluded from the mutation gate but are neither a "
            "bench nor a crate-root build script, so they are modules leaving "
            "the gate unnoticed: " + ", ".join(offenders),
        )

    def test_the_graph_builder_module_is_inside_the_gate(self):
        """The regression this contract was written for.

        `crates/pixel-graph/src/build.rs` is a source module, not a build
        script. A glob that excludes it hides `build_graph`, `tree_delta`,
        `apply_tree_delta` and the freshness signature from the gate.
        """
        module = "crates/pixel-graph/src/build.rs"
        self.assertIn(module, rust_files(), "the module moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        hit = [g for g, p in zip(exclude_globs(), patterns) if p.match(module)]
        self.assertEqual(hit, [], f"{module} is excluded by {hit}")

    def test_the_cli_build_script_stays_excluded(self):
        """The exclusion the config is actually for keeps working."""
        script = "crates/pixel/build.rs"
        self.assertIn(script, rust_files(), "the build script moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        self.assertTrue(
            any(p.match(script) for p in patterns),
            f"{script} is a cargo build script and should stay excluded",
        )

    def test_the_glob_translation_separates_star_from_double_star(self):
        """`*` must not cross a separator, or the contract above proves nothing."""
        single = to_regex("crates/*/build.rs")
        self.assertTrue(single.match("crates/pixel/build.rs"))
        self.assertFalse(single.match("crates/pixel-graph/src/build.rs"))
        double = to_regex("**/build.rs")
        self.assertTrue(double.match("crates/pixel/build.rs"))
        self.assertTrue(double.match("crates/pixel-graph/src/build.rs"))
        self.assertTrue(double.match("build.rs"))
        self.assertTrue(to_regex("crates/pixel-bench/**").match("crates/pixel-bench/a/b.rs"))
        self.assertFalse(to_regex("crates/pixel-bench/**").match("crates/pixel/a.rs"))


#: Every file that runs `cargo mutants` (not only lists them).
LANES = [
    ".github/workflows/mutants.yml",
    ".github/workflows/mutants-nightly.yml",
    "scripts/mutants-preflight.sh",
    "scripts/gates.sh",
]

#: Arguments that change which mutants compile, which tests judge them or
#: when one times out. A lane that passes one alone runs another program
#: than the others (find-my-files#204 measured 40 mutants unviable locally
#: and built by CI for one such flag), so they belong in .cargo/mutants.toml
#: or nowhere.
LANE_ONLY_FORBIDDEN = [
    "--all-targets", "--locked", "-C", "--cargo-arg", "--cargo-test-arg",
    "--test-tool", "--test-workspace", "--timeout", "--timeout-multiplier",
    "--minimum-test-timeout", "--build-timeout", "--features",
    "--all-features", "--no-default-features", "--profile", "--baseline",
]


def yaml_run_blocks(text: str) -> list[str]:
    """The shell of every `run:` in a workflow, as the runner receives it.

    A folded scalar (`run: >`) is one line, every continuation line joined
    with a space however many there are; a literal one (`run: |`) keeps its
    lines. Comments and every other key are dropped, so a flag named in a
    comment is not a flag passed.
    """
    lines = text.splitlines()
    blocks = []
    i = 0
    while i < len(lines):
        block = re.match(r"^(\s*)(?:- )?run:\s*([>|])[-+]?\s*$", lines[i])
        inline = re.match(r"^\s*(?:- )?run:\s*(\S.*)$", lines[i])
        i += 1
        if block:
            indent = len(block.group(1))
            body = []
            while i < len(lines) and (not lines[i].strip() or len(lines[i]) - len(lines[i].lstrip()) > indent):
                body.append(lines[i].strip())
                i += 1
            if block.group(2) == ">":
                # Folding joins adjacent lines with a space, but a blank
                # line is a line break: two commands, not one.
                paragraphs, current = [], []
                for part in body:
                    if part:
                        current.append(part)
                    elif current:
                        paragraphs.append(" ".join(current))
                        current = []
                if current:
                    paragraphs.append(" ".join(current))
                blocks.append("\n".join(paragraphs))
            else:
                blocks.append("\n".join(body))
        elif inline:
            blocks.append(inline.group(1))
    return blocks


def lane_shell(lane: str, text: str) -> str:
    """The shell a lane runs: a workflow's `run:` blocks, a script whole."""
    return "\n".join(yaml_run_blocks(text)) if lane.endswith((".yml", ".yaml")) else text


def cargo_mutants_runs(text: str) -> list[str]:
    """Each `cargo mutants` invocation that runs mutants, backslash
    continuations joined. `--list` and `--version` invocations build nothing
    and are left out. A workflow goes through `lane_shell` first."""
    joined = re.sub(r"\\\n\s*", " ", text)
    runs = []
    for line in joined.splitlines():
        code = line.split(" #", 1)[0]
        if "cargo mutants" not in code or code.lstrip().startswith("#"):
            continue
        tail = code.split("cargo mutants", 1)[1]
        if "--list" in tail or "--version" in tail:
            continue
        runs.append(tail)
    return runs


class OneProgramForEveryLane(unittest.TestCase):
    """CI's shards and the local lanes must run one program on a mutant."""

    def config(self) -> str:
        return CONFIG.read_text()

    def test_the_config_carries_the_arguments_every_lane_shares(self):
        text = self.config()
        self.assertRegex(text, r'(?m)^additional_cargo_args = \["--locked"\]$')
        self.assertRegex(text, r'(?m)^additional_cargo_test_args = \["--all-targets"\]$')

    def test_every_lane_is_found(self):
        for lane in LANES:
            with self.subTest(lane=lane):
                self.assertTrue(cargo_mutants_runs(lane_shell(lane, (REPO / lane).read_text())),
                                f"{lane} no longer runs cargo mutants; update LANES")

    def test_no_lane_passes_an_argument_that_changes_the_program(self):
        for lane in LANES:
            for run in cargo_mutants_runs(lane_shell(lane, (REPO / lane).read_text())):
                words = set(re.findall(r"(?<![\w-])-{1,2}[\w-]+", run))
                with self.subTest(lane=lane, run=run.strip()):
                    self.assertEqual(sorted(words & set(LANE_ONLY_FORBIDDEN)), [])

    def test_the_forbidden_list_catches_the_arguments_ci_used_to_pass(self):
        before = "cargo mutants -vV -C --locked --no-shuffle --in-place --in-diff pr.diff \\\n  --shard 0/2 -- --all-targets\n"
        runs = cargo_mutants_runs(before)
        self.assertEqual(len(runs), 1)
        words = set(re.findall(r"(?<![\w-])-{1,2}[\w-]+", runs[0]))
        self.assertEqual(sorted(words & set(LANE_ONLY_FORBIDDEN)), ["--all-targets", "--locked", "-C"])

    def test_every_line_of_a_folded_command_is_inspected(self):
        workflow = (
            "      - name: Run\n"
            "        run: >\n"
            "          cargo mutants -vV --no-shuffle --in-place --in-diff pr.diff\n"
            "          --shard \"$SHARD\"\n"
            "          -- --all-targets 2>&1 | tee results.txt\n"
            "      - name: Next\n"
            "        run: echo done\n"
        )
        runs = cargo_mutants_runs(lane_shell("x.yml", workflow))
        self.assertEqual(len(runs), 1)
        self.assertIn("--all-targets", runs[0])
        self.assertNotIn("echo done", runs[0])

    def test_a_blank_line_in_a_folded_block_separates_two_commands(self):
        workflow = (
            "      - name: Run\n"
            "        run: >\n"
            "          cargo mutants --version\n"
            "\n"
            "          cargo mutants --in-diff pr.diff\n"
            "          -- --all-targets\n"
        )
        runs = cargo_mutants_runs(lane_shell("x.yml", workflow))
        self.assertEqual(len(runs), 1)
        self.assertIn("--all-targets", runs[0])

    def test_a_flag_named_in_a_workflow_comment_is_not_a_flag_passed(self):
        workflow = (
            "      # `--all-targets` comes from .cargo/mutants.toml\n"
            "      - name: Run\n"
            "        run: cargo mutants --in-diff pr.diff\n"
        )
        runs = cargo_mutants_runs(lane_shell("x.yml", workflow))
        self.assertEqual([r.strip() for r in runs], ["--in-diff pr.diff"])

    def test_every_ci_run_reads_the_workflow_s_own_configuration(self):
        # A dispatch mutates a tip whose .cargo/mutants.toml may predate the
        # shared arguments: the listing and every shard read the plan's copy.
        for lane in (".github/workflows/mutants.yml",):
            text = (REPO / lane).read_text()
            shell = lane_shell(lane, text)
            with self.subTest(lane=lane):
                self.assertIn("cp .cargo/mutants.toml mutants-config.toml", shell)
                for run in cargo_mutants_runs(shell):
                    self.assertIn("--config mutants-config.toml", run)
                listing = [l for l in re.sub(r"\\\n\s*", " ", shell).splitlines() if "cargo mutants --list" in l]
                self.assertTrue(listing and all('--config "$PWD/mutants-config.toml"' in l for l in listing), listing)

    def test_a_listing_is_not_a_run(self):
        self.assertEqual(cargo_mutants_runs('cargo mutants --list --in-diff "$d" > out\ncargo mutants --version\n'), [])

    def test_the_local_lanes_check_the_pinned_version_before_running(self):
        for lane in ("scripts/mutants-preflight.sh", "scripts/gates.sh"):
            text = (REPO / lane).read_text()
            with self.subTest(lane=lane):
                check = text.find("mutants-version-check.sh")
                self.assertNotEqual(check, -1)
                self.assertLess(check, text.find("cargo mutants -", check) if "cargo mutants -" in text[check:] else len(text))

    def test_every_job_pins_the_same_cargo_mutants(self):
        pins = set()
        for workflow in (".github/workflows/mutants.yml", ".github/workflows/mutants-nightly.yml"):
            pins |= set(re.findall(r"tool: cargo-mutants@(\S+)", (REPO / workflow).read_text()))
        self.assertEqual(len(pins), 1, pins)

    def test_ci_mutants_inherit_created_runner_temp_and_preserve_failure_status(self):
        workflow = (REPO / ".github/workflows/mutants.yml").read_text()

        def step_shell(name):
            step = workflow.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
            blocks = yaml_run_blocks(step)
            self.assertEqual(len(blocks), 1)
            return blocks[0]

        prepare = step_shell("Use disk-backed mutation temporary storage")
        run = step_shell("Run this shard's mutants")
        self.assertLess(
            workflow.index("name: Use disk-backed mutation temporary storage"),
            workflow.index("name: Run this shard's mutants"),
        )
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-temp-") as tmp:
            root = Path(tmp)
            runner_temp = root / "runner temp"
            # Outside RUNNER_TEMP: on the self-hosted runner it lies in a
            # $HOME that is a git repository, which breaks every test that
            # needs a scratch directory outside git (#599).
            scratch_base = root / "var tmp"
            scratch_base.mkdir()
            scratch = scratch_base / "pixel-mutants-41-2-0-of-10"
            expected = scratch / "tmp"
            github_env = root / "github-env"
            probe = root / "inherited-temp"
            bin_dir = root / "bin"
            bin_dir.mkdir()
            # Exercise the workflow shell without building or executing mutants.
            cargo = bin_dir / "cargo"
            cargo.write_text(
                '#!/bin/sh\n'
                'test -d "$TMPDIR" || exit 91\n'
                'printf "%s\\n" "$TMPDIR" > "$PROBE_FILE"\n'
                'printf "temporary output\\n" > "$TMPDIR/linker-probe"\n'
                'exit "$STUB_CARGO_EXIT"\n'
            )
            cargo.chmod(0o755)
            env = dict(os.environ)
            env.pop("TMPDIR", None)
            env.update(
                RUNNER_TEMP=str(runner_temp),
                GITHUB_ENV=str(github_env),
                GITHUB_RUN_ID="41",
                GITHUB_RUN_ATTEMPT="2",
                SHARD="0/10",
                PIXEL_MUTANTS_SCRATCH_BASE=str(scratch_base),
            )
            result = subprocess.run(
                ["bash", "-e", "-o", "pipefail", "-c", prepare],
                cwd=root, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(expected.is_dir())
            runtime = scratch / "runtime"
            self.assertTrue(runtime.is_dir())
            self.assertEqual(runtime.stat().st_mode & 0o777, 0o700)
            self.assertFalse(runner_temp.exists(), "nothing lands under RUNNER_TEMP")
            updates = dict(line.split("=", 1) for line in github_env.read_text().splitlines())
            self.assertEqual(
                updates,
                {
                    "PIXEL_MUTANTS_SCRATCH": str(scratch),
                    "TMPDIR": str(expected),
                    "XDG_RUNTIME_DIR": str(runtime),
                },
            )
            env.update(updates)
            env.update(PATH=f"{bin_dir}{os.pathsep}{env['PATH']}", PROBE_FILE=str(probe), SHARD="0/10")
            for status in (0, 7):
                with self.subTest(cargo_exit=status):
                    env["STUB_CARGO_EXIT"] = str(status)
                    result = subprocess.run(
                        ["bash", "-e", "-o", "pipefail", "-c", run],
                        cwd=root, env=env, capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, status, result.stderr)
                    self.assertEqual(probe.read_text(), f"{expected}\n")
                    self.assertEqual((expected / "linker-probe").read_text(), "temporary output\n")


class DispatchRangeStaysInTheDispatchedBranch(unittest.TestCase):
    """A dispatched run mutates only a commit the dispatched branch holds.

    The shards build and test the range's right end inside the run's cache
    scope, `main`'s when dispatched from it. A tip on an unmerged branch
    would run unreviewed code where it can write cache entries every later
    `main` run restores (CodeQL actions/cache-poisoning/poisonable-step,
    #674). The plan job's diff step is the one place that can refuse it,
    before any worktree or `ref` output reaches the shards.
    """

    @staticmethod
    def diff_step() -> str:
        workflow = (REPO / ".github/workflows/mutants.yml").read_text()
        step = workflow.split("      - name: Diff against the target branch\n", 1)[1]
        blocks = yaml_run_blocks(step.split("\n      - ", 1)[0])
        assert len(blocks) == 1, blocks
        return blocks[0]

    def dispatch(self, base: str, tip: str, repo: Path, root: Path):
        output = root / "github-output"
        output.write_text("")
        env = dict(os.environ)
        env.update(
            GITHUB_EVENT_NAME="workflow_dispatch",
            DIFF_RANGE=f"{base}...{tip}",
            GITHUB_OUTPUT=str(output),
            GITHUB_REF_NAME="main",
            RUNNER_TEMP=str(root / "runner"),
        )
        result = subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", self.diff_step()],
            cwd=repo, env=env, capture_output=True, text=True,
        )
        return result, output.read_text()

    def test_only_a_tip_in_the_dispatched_branch_history_reaches_the_shards(self):
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-dispatch-") as tmp:
            root = Path(tmp)
            repo = root / "repo"
            (root / "runner").mkdir()
            env = dict(
                os.environ,
                GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com",
                GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
            )

            def git(*args: str) -> str:
                return subprocess.run(
                    ["git", "-C", str(repo), *args],
                    env=env, check=True, capture_output=True, text=True,
                ).stdout.strip()

            def commit(message: str) -> str:
                (repo / "crates").mkdir(exist_ok=True)
                (repo / "crates" / "lib.rs").write_text(f"// {message}\n")
                git("add", ".")
                git("commit", "-qm", message)
                return git("rev-parse", "HEAD")

            repo.mkdir()
            git("init", "-q", "-b", "main")
            base = commit("base")
            git("switch", "-qc", "unmerged")
            outside = commit("unmerged work")
            git("switch", "-q", "main")
            merged = commit("merged work")

            result, outputs = self.dispatch(base, outside, repo, root)
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("Range outside this branch", result.stdout)
            self.assertNotIn("ref=", outputs, "no ref may reach the shards")
            self.assertFalse((root / "runner" / "tip").exists())

            result, outputs = self.dispatch(base, merged, repo, root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn(f"ref={merged}\n", outputs)
            self.assertEqual(
                (root / "repo" / "pr.diff").read_text().count("+++ b/crates/lib.rs"), 1
            )


class MutantsGateReport(unittest.TestCase):
    """Contract of scripts/mutants-gate.py.

    The defect it answers: `cargo mutants --in-diff` exits 0 when it produced
    no mutants, so a pull request whose every file is excluded shows a green
    mutation gate over code nothing mutated. #203 did exactly that -- green in
    56 s, `No files were found with the provided path: mutants.out`. The
    report has to separate "0 missed out of N tested" from "0 tested".
    """

    def run_gate(self, diff: str, listing: str, *extra: str):
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-gate-") as tmp:
            root = Path(tmp)
            (root / "pr.diff").write_text(diff)
            (root / "list.txt").write_text(listing)
            return subprocess.run(
                [
                    sys.executable,
                    str(GATE),
                    "--diff",
                    str(root / "pr.diff"),
                    "--list",
                    str(root / "list.txt"),
                    *extra,
                ],
                capture_output=True,
                text=True,
                check=False,
            )

    @staticmethod
    def diff_touching(*paths: str) -> str:
        return "".join(f"--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-a\n+b\n" for p in paths)

    MUTANT_LINE = (
        "crates/pixel-graph/src/build.rs:407:5: replace tree_hashes -> "
        "Vec<(String, u64)> with vec![]\n"
    )

    def test_a_diff_that_produced_mutants_is_reported_as_tested(self):
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"),
            self.MUTANT_LINE * 10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("**mutants tested:** 10", result.stdout)
        self.assertIn("`tested`", result.stdout)
        self.assertNotIn("::warning", result.stdout)

    def test_rust_changed_with_zero_mutants_is_reported_as_vacuous(self):
        """The #203 shape: the gate is green and proves nothing."""
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"), ""
        )
        self.assertIn("**mutants tested:** 0", result.stdout)
        self.assertIn("`vacuous`", result.stdout)
        self.assertIn("proves nothing", result.stdout)
        self.assertIn("crates/pixel-graph/src/build.rs", result.stdout)
        self.assertIn("::warning title=Mutation gate tested nothing::", result.stdout)
        self.assertEqual(result.returncode, 0, "warns by default, never blocks")

    def test_the_vacuous_case_can_be_made_a_hard_failure(self):
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"),
            "",
            "--fail-on-vacuous",
        )
        self.assertEqual(result.returncode, 1)

    def test_a_diff_with_no_mutable_rust_owes_no_mutants(self):
        """Docs, benches and crate-root build scripts must not be flagged."""
        for paths in (
            ("README.md", "docs/bench/tree-delta.md"),
            ("crates/pixel-bench/benches/tree_delta.rs",),
            ("crates/pixel/build.rs",),
            ("Cargo.toml", "changelog.d/204-x.fixed.md"),
        ):
            with self.subTest(paths=paths):
                result = self.run_gate(self.diff_touching(*paths), "")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("`not-applicable`", result.stdout)
                self.assertNotIn("::warning", result.stdout)

    def test_a_deleted_file_is_not_counted_as_changed_rust(self):
        deletion = "--- a/crates/pixel-graph/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n"
        result = self.run_gate(deletion, "")
        self.assertIn("`not-applicable`", result.stdout)


class ShardedMutantsGate(unittest.TestCase):
    """The two roles scripts/mutants-gate.py plays around the shard matrix.

    Sizing: the plan job turns the listing into `--shard k/n` values. A gap
    in that list is a slice of the diff no job ever mutates, and a value of
    `n/n` makes cargo-mutants refuse to start. Totalling: once the shards
    finish, the gate totals their `outcomes.json`. The failure it exists for
    is a shard that crashed or was cancelled. Its mutants were never judged,
    and without a check the shards that did finish would add up to a pass.
    """

    DIFF = "--- a/crates/pixel/src/main.rs\n+++ b/crates/pixel/src/main.rs\n@@ -1 +1 @@\n-a\n+b\n"

    @staticmethod
    def listing(count: int) -> str:
        return "".join(
            f"crates/pixel/src/main.rs:{line}:5: replace f{line} -> bool with true\n"
            for line in range(1, count + 1)
        )

    @staticmethod
    def write_shard(root: Path, name: str, *summaries: str, logs=None) -> None:
        """One shard's mutants.out, shaped like cargo-mutants 27.1 writes it.

        `logs` maps a mutant's 1-based index to the text of its build log,
        written under `log/` and referenced by `log_path` as cargo-mutants does.
        """
        logs = logs or {}
        (root / name / "log").mkdir(parents=True)
        outcomes = [{"scenario": "Baseline", "summary": "Success"}]
        for i, summary in enumerate(summaries, start=1):
            outcome = {
                "scenario": {"Mutant": {"name": f"crates/pixel/src/main.rs:{i}:5: {name} #{i}"}},
                "summary": summary,
            }
            if i in logs:
                outcome["log_path"] = f"log/mutant_{i}.log"
                (root / name / outcome["log_path"]).write_text(logs[i])
            outcomes.append(outcome)
        (root / name / "outcomes.json").write_text(json.dumps({"outcomes": outcomes}))

    def run_gate(self, listed: int, *extra: str, shards=()):
        """Run the gate on `listed` mutants; `shards` are `(name, summaries)` pairs."""
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-shards-") as tmp:
            root = Path(tmp)
            (root / "pr.diff").write_text(self.DIFF)
            (root / "list.txt").write_text(self.listing(listed))
            for name, summaries, *logs in shards:
                self.write_shard(root / "shards", name, *summaries, logs=logs[0] if logs else None)
            output = root / "github-output"
            result = subprocess.run(
                [
                    sys.executable,
                    str(GATE),
                    "--diff",
                    str(root / "pr.diff"),
                    "--list",
                    str(root / "list.txt"),
                    "--github-output",
                    str(output),
                    *[a.replace("{root}", tmp) for a in extra],
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            outputs = dict(
                line.split("=", 1) for line in output.read_text().splitlines()
            )
            return result, outputs

    def run_gate_raw(self, listed: int, *extra: str, shards=()):
        """`run_gate` without reading `github-output`: for a gate that exits
        before writing it (a malformed `--runners` file)."""
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-shards-") as tmp:
            root = Path(tmp)
            (root / "pr.diff").write_text(self.DIFF)
            (root / "list.txt").write_text(self.listing(listed))
            for name, summaries, *logs in shards:
                self.write_shard(root / "shards", name, *summaries, logs=logs[0] if logs else None)
            output = root / "github-output"
            return subprocess.run(
                [
                    sys.executable,
                    str(GATE),
                    "--diff",
                    str(root / "pr.diff"),
                    "--list",
                    str(root / "list.txt"),
                    "--github-output",
                    str(output),
                    *[a.replace("{root}", tmp) for a in extra],
                ],
                capture_output=True,
                text=True,
                check=False,
            )

    def shards_for(self, listed: int) -> list[str]:
        result, outputs = self.run_gate(listed)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["mutants"], str(listed))
        return json.loads(outputs["shards"])

    def matrix_for(self, listed: int, *extra: str, pool: str | None = None):
        """The gate's `matrix` output as the workflow reads it.

        `pool` becomes the `--runners` file's content: the
        PIXEL_MUTANTS_SHARD_RUNNERS variable the plan job passes on.
        """
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-matrix-") as tmp:
            extra = list(extra)
            if pool is not None:
                runners = Path(tmp) / "runners.json"
                runners.write_text(pool)
                extra += ["--runners", str(runners)]
            result, outputs = self.run_gate(listed, *extra)
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(outputs["matrix"])["include"]

    def test_a_diff_without_mutants_starts_no_shard(self):
        """Every shard pays a baseline build, so nothing to mutate means no job."""
        self.assertEqual(self.shards_for(0), [])

    def test_shards_cover_every_slice_and_stay_within_cargo_mutants_range(self):
        for listed in (1, 20, 21, 95, 10_000):
            with self.subTest(listed=listed):
                shards = self.shards_for(listed)
                count = len(shards)
                self.assertEqual(shards, [f"{k}/{count}" for k in range(count)])

    def test_a_small_diff_keeps_one_baseline_and_a_large_one_is_split(self):
        self.assertEqual(len(self.shards_for(10)), 1)
        self.assertEqual(len(self.shards_for(11)), 2)
        self.assertGreater(len(self.shards_for(95)), 1)

    def test_a_shard_holds_at_most_ten_mutants_below_the_job_budget(self):
        """The pull request waits for the slowest shard: past 10 `pixel-cli`
        mutants, one shard runs longer than the setup and baseline a second
        one would add."""
        for listed in (10, 32, 100):
            with self.subTest(listed=listed):
                self.assertEqual(len(self.shards_for(listed)), -(-listed // 10))

    def test_a_huge_diff_does_not_exceed_the_concurrent_job_budget(self):
        """A free account runs 20 jobs at once; the CI workflow needs some of them."""
        self.assertLessEqual(len(self.shards_for(10_000)), 20)

    def test_the_matrix_pairs_every_shard_with_the_default_runner(self):
        """No PIXEL_MUTANTS_SHARD_RUNNERS: every shard stays on GitHub's image."""
        include = self.matrix_for(25)
        self.assertEqual(
            include,
            [
                {"shard": f"{k}/3", "runner": "ubuntu-26.04"} for k in range(3)
            ],
        )

    def test_the_pool_is_dealt_round_robin_one_host_per_shard(self):
        """A pool of two spreads the matrix evenly, shard k taking pool k % len.

        Each shard sits on exactly one host: its mutants are consecutive
        slices, and a host holds no shard half of the list.
        """
        include = self.matrix_for(21, pool='["a2","b1"]')
        self.assertEqual(
            [entry["runner"] for entry in include],
            ["a2", "b1", "a2"],
        )
        self.assertEqual(
            [entry["shard"] for entry in include], ["0/3", "1/3", "2/3"]
        )

    def test_a_pool_larger_than_the_matrix_leaves_no_idle_entry(self):
        """More hosts than shards: the include list stays shard-sized."""
        include = self.matrix_for(11, pool='["a2","b1","c3","d4"]')
        self.assertEqual(
            [entry["runner"] for entry in include], ["a2", "b1"]
        )

    def test_an_empty_or_blank_pool_falls_back_to_the_default(self):
        """The variable set but empty must not ask GitHub for a runner with
        empty labels: it would queue forever."""
        for pool in ("", "[]", '["", "  "]'):
            with self.subTest(pool=pool):
                include = self.matrix_for(2, pool=pool)
                self.assertEqual(
                    [entry["runner"] for entry in include], ["ubuntu-26.04"]
                )

    def test_a_malformed_pool_fails_the_plan_loudly(self):
        """A mistyped variable must not silently route everything to the
        default: the plan job names the bad value and stops the run."""
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-bad-pool-") as tmp:
            runners = Path(tmp) / "runners.json"
            runners.write_text('{"a2": 8}')
            result = self.run_gate_raw(2, "--runners", str(runners))
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error title=Bad PIXEL_MUTANTS_SHARD_RUNNERS::", result.stdout)
        self.assertIn("expected a JSON array of runs-on values", result.stdout)

    def test_a_pool_with_a_non_string_member_fails_the_plan(self):
        """`["a2", 8]`: a number is a mistyped variable, and a `null` dropped
        here would route its shards somewhere nobody configured."""
        for pool in ('["a2", 8]', '["a2", null]', '[12]'):
            with self.subTest(pool=pool):
                with tempfile.TemporaryDirectory(prefix="pixel-mutants-bad-member-") as tmp:
                    runners = Path(tmp) / "runners.json"
                    runners.write_text(pool)
                    result = self.run_gate_raw(2, "--runners", str(runners))
                self.assertEqual(result.returncode, 1)
                self.assertIn(
                    "::error title=Bad PIXEL_MUTANTS_SHARD_RUNNERS::", result.stdout
                )
                self.assertIn("pool member is not a string", result.stdout)

    def test_every_listed_mutant_caught_or_unviable_passes(self):
        result, _ = self.run_gate(
            4,
            "--outcomes-root",
            "{root}/shards",
            shards=[
                ("mutants-out-0", ("CaughtMutant", "Unviable")),
                ("mutants-out-1", ("CaughtMutant", "CaughtMutant")),
            ],
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("4 judged: 3 caught, 0 missed, 0 timeout, 1 unviable", result.stdout)
        self.assertIn("**gate:** passed", result.stdout)

    def test_a_single_shard_extracted_at_the_root_is_totalled(self):
        """`download-artifact` puts a lone matching artifact straight into its path.

        With one shard there is no `shards/mutants-out-0/` directory: the
        artifact's files land in `shards/` itself. Reading only
        `shards/*/outcomes.json` then totals nothing, and every PR of 20
        mutants or fewer failed with "0 reached a verdict" (#225).
        """
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-shards-") as tmp:
            self.write_shard(Path(tmp), "shards", "CaughtMutant", "Unviable")
            result, _ = self.run_gate(2, "--outcomes-root", f"{tmp}/shards")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 judged: 1 caught, 0 missed, 0 timeout, 1 unviable", result.stdout)

    def test_a_survivor_in_any_shard_fails_the_gate_and_is_named(self):
        for summary, label in (("MissedMutant", "MISSED"), ("Timeout", "TIMEOUT")):
            with self.subTest(summary=summary):
                result, _ = self.run_gate(
                    3,
                    "--outcomes-root",
                    "{root}/shards",
                    shards=[
                        ("mutants-out-0", ("CaughtMutant",)),
                        ("mutants-out-1", ("CaughtMutant", summary)),
                    ],
                )
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"{label} crates/pixel/src/main.rs:2:5: mutants-out-1 #2", result.stdout)
                self.assertIn("::error title=Mutation gate failed::", result.stdout)

    def test_an_outcome_the_gate_does_not_know_fails_closed(self):
        """A summary a later cargo-mutants adds must not count as caught."""
        result, _ = self.run_gate(
            2,
            "--outcomes-root",
            "{root}/shards",
            shards=[("mutants-out-0", ("CaughtMutant", "Failure"))],
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("1 mutant(s) ended with an outcome the gate does not know (Failure)", result.stdout)
        self.assertIn("FAILURE crates/pixel/src/main.rs:2:5: mutants-out-0 #2", result.stdout)
        self.assertIn("**gate:** failed", result.stdout)

    def test_a_shard_that_left_no_outcomes_fails_the_gate(self):
        """Two shards caught everything they ran; the third never reported."""
        result, _ = self.run_gate(
            6,
            "--outcomes-root",
            "{root}/shards",
            shards=[
                ("mutants-out-0", ("CaughtMutant", "CaughtMutant")),
                ("mutants-out-1", ("CaughtMutant", "CaughtMutant")),
            ],
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("6 mutant(s) listed, 4 reached a verdict", result.stdout)

    def test_a_shard_whose_baseline_failed_counts_as_unjudged(self):
        result, _ = self.run_gate(
            2, "--outcomes-root", "{root}/shards", shards=[("mutants-out-0", ())]
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("2 mutant(s) listed, 0 reached a verdict", result.stdout)

    def test_the_baseline_is_not_counted_as_a_mutant(self):
        """Counting it would turn every complete run into a mismatch."""
        result, _ = self.run_gate(
            1,
            "--outcomes-root",
            "{root}/shards",
            shards=[("mutants-out-0", ("CaughtMutant",))],
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 judged", result.stdout)

    def test_an_unviable_mutant_whose_build_ran_out_of_disk_was_never_judged(self):
        """PR #222's run filled the runner's disk at its 101st mutant.

        Every later build failed at the link step, cargo-mutants reported
        each one `unviable`, and a gate that holds `unviable` called a run
        whose last 546 mutants never compiled `tested`. A genuine unviable
        (a rustc type error) in the same shard must still pass.
        """
        result, _ = self.run_gate(
            3,
            "--outcomes-root",
            "{root}/shards",
            shards=[(
                "mutants-out-0",
                ("CaughtMutant", "Unviable", "Unviable"),
                {
                    2: "error[E0277]: the trait bound `Response: Default` is not satisfied\n",
                    3: "cc: error: Cannot create temporary file in /tmp/: No space left on device\n",
                },
            )],
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(
            "3 judged: 1 caught, 0 missed, 0 timeout, 1 unviable, 1 disk-full", result.stdout
        )
        self.assertIn("1 mutant(s) reported unviable because the runner's disk filled up", result.stdout)
        self.assertIn("DISK-FULL crates/pixel/src/main.rs:3:5: mutants-out-0 #3", result.stdout)
        self.assertNotIn("#2", result.stdout)

    def test_an_unviable_mutant_without_a_log_stays_unviable(self):
        """No log is no evidence of a full disk: the outcome is taken as reported."""
        result, _ = self.run_gate(
            1, "--outcomes-root", "{root}/shards", shards=[("mutants-out-0", ("Unviable",))]
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 judged: 0 caught, 0 missed, 0 timeout, 1 unviable, 0 disk-full", result.stdout)

    @unittest.skipUnless(
        os.environ.get("MUTANTS_REPLAY_PR222"),
        "set MUTANTS_REPLAY_PR222 to `gh run download 35852843025 -n mutants-out -D <dir>`",
    )
    def test_the_pr222_run_replays_as_546_mutants_lost_to_a_full_disk(self):
        """The run that motivated the check, replayed from its own artifact."""
        spec = importlib.util.spec_from_file_location("mutants_gate", GATE)
        gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(gate)
        counts, _ = gate.tally(Path(os.environ["MUTANTS_REPLAY_PR222"]))
        self.assertEqual(counts[gate.DISK_FULL], 546)
        self.assertEqual(counts["unviable"], 20)
        self.assertEqual((counts["caught"], counts["missed"], counts["timeout"]), (67, 24, 1))
        self.assertIn("disk filled up", gate.outcome_failure(658, counts))

    def test_no_mutants_and_no_shards_is_not_a_failure(self):
        result, _ = self.run_gate(0, "--outcomes-root", "{root}/shards")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("**gate:** passed", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
