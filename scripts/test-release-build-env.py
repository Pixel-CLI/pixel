#!/usr/bin/env python3
"""Contract of the reproducible release build (scripts/release-build-env.sh).

A release binary is reproducible only if every input that varies between two
builds of one commit is pinned: the build date crates/pixel/build.rs embeds
(SOURCE_DATE_EPOCH, from the commit), the checkout and CARGO_HOME paths rustc
writes into panic messages and debug information (--remap-path-prefix), and
the cross image (its version). reproducible-build.yml proves it by building
twice; these cases hold what that proof depends on and what a later edit
could silently drop:

- the script's output: the commit's time, both remaps, an existing RUSTFLAGS
  kept, a path RUSTFLAGS cannot carry refused before anything is written;
- Cross.toml forwards SOURCE_DATE_EPOCH, which cross does not by itself;
- release-build.yml, cross-build.yml and reproducible-build.yml set the
  environment before the cache step and the build, pin the same cross, and
  reproducible-build.yml builds with release-build.yml's exact command.
"""
from __future__ import annotations

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "release-build-env.sh"
WORKFLOWS = ROOT / ".github" / "workflows"
EPOCH = 1700000000


def git(repo: Path, *args: str, env: dict | None = None) -> None:
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, env=env)


def make_repo(path: Path) -> None:
    path.mkdir(parents=True)
    git(path, "init", "-q")
    (path / "f").write_text("x\n")
    git(path, "add", "f")
    env = dict(
        os.environ,
        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com",
        GIT_AUTHOR_DATE=f"@{EPOCH - 500} +0000",
        GIT_COMMITTER_DATE=f"@{EPOCH} +0000",
    )
    git(path, "commit", "-q", "-m", "c", env=env)


def run(cwd: Path, *args: str, **env: str) -> subprocess.CompletedProcess:
    base = {k: v for k, v in os.environ.items() if k not in ("RUSTFLAGS", "GITHUB_ENV")}
    return subprocess.run(
        ["sh", str(SCRIPT), *args], cwd=cwd, env={**base, **env},
        capture_output=True, text=True,
    )


def steps(workflow: str) -> list[str]:
    """The workflow's text cut at each `- name:` / `- uses:` step start."""
    text = (WORKFLOWS / workflow).read_text()
    return re.split(r"\n\s*- (?=name:|uses:)", text)


def index_of(parts: list[str], needle: str) -> int:
    hits = [i for i, p in enumerate(parts) if needle in p]
    if not hits:
        raise AssertionError(f"no step contains {needle!r}")
    return hits[0]


class ScriptOutput(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = Path(self.tmp.name).resolve()
        self.repo = self.base / "checkout"
        make_repo(self.repo)
        self.cargo_home = str(self.base / "cargo-home")

    def tearDown(self):
        self.tmp.cleanup()

    def test_github_env_pins_commit_time_and_remaps_both_paths(self):
        out = self.base / "github_env"
        out.write_text("EARLIER=1\n")
        res = run(self.repo, "--github-env", GITHUB_ENV=str(out), CARGO_HOME=self.cargo_home)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(
            out.read_text().splitlines(),
            [
                "EARLIER=1",
                # the committer time, not the author time nor now
                f"SOURCE_DATE_EPOCH={EPOCH}",
                f"RUSTFLAGS=--remap-path-prefix={self.repo}=/pixel "
                f"--remap-path-prefix={self.cargo_home}=/cargo",
            ],
        )

    def test_shell_mode_exports_and_keeps_existing_rustflags(self):
        res = run(self.repo, CARGO_HOME=self.cargo_home, RUSTFLAGS="-C target-cpu=x86-64-v2")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(
            res.stdout.splitlines(),
            [
                f"export SOURCE_DATE_EPOCH='{EPOCH}'",
                f"export RUSTFLAGS='-C target-cpu=x86-64-v2 --remap-path-prefix={self.repo}=/pixel "
                f"--remap-path-prefix={self.cargo_home}=/cargo'",
            ],
        )

    def test_checkout_path_is_canonical(self):
        # rustc sees the physical path; a remap of a symlinked spelling
        # would match nothing and leave the real path in the binary.
        link = self.base / "link"
        link.symlink_to(self.repo)
        # PWD as a shell that cd'ed through the link would leave it.
        res = run(link, CARGO_HOME=self.cargo_home, PWD=str(link))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn(f"--remap-path-prefix={self.repo}=/pixel", res.stdout)
        self.assertNotIn(f"{link}=", res.stdout)

    def test_run_from_a_subdirectory_remaps_the_checkout_root(self):
        # cargo compiles every crate of the checkout, not only the caller's
        # directory: a remap of the subdirectory leaves the root's path in.
        sub = self.repo / "crates" / "pixel"
        sub.mkdir(parents=True)
        res = run(sub, CARGO_HOME=self.cargo_home, PWD=str(sub))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn(f"--remap-path-prefix={self.repo}=/pixel ", res.stdout)
        self.assertNotIn(f"{sub}=", res.stdout)

    def test_outside_a_checkout_fails_without_output(self):
        outside = self.base / "not-a-repo"
        outside.mkdir()
        res = run(outside, CARGO_HOME=self.cargo_home, GIT_CEILING_DIRECTORIES=str(self.base))
        self.assertNotEqual(res.returncode, 0)
        self.assertEqual(res.stdout, "")

    def test_path_with_whitespace_is_refused_before_writing(self):
        spaced = self.base / "with space"
        make_repo(spaced)
        out = self.base / "github_env"
        res = run(spaced, "--github-env", GITHUB_ENV=str(out), CARGO_HOME=self.cargo_home)
        self.assertEqual(res.returncode, 1)
        self.assertIn("whitespace", res.stderr)
        self.assertFalse(out.exists())

    def test_cargo_home_with_quote_is_refused(self):
        res = run(self.repo, CARGO_HOME=str(self.base / "it's"))
        self.assertEqual(res.returncode, 1)
        self.assertEqual(res.stdout, "")

    def test_unknown_argument_is_refused(self):
        res = run(self.repo, "--github", CARGO_HOME=self.cargo_home)
        self.assertEqual(res.returncode, 2)
        self.assertEqual(res.stdout, "")


def run_block(workflow: str, step: str) -> str:
    """The `run: |` block of the step named `step`, dedented."""
    lines = (WORKFLOWS / workflow).read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {step}")
    run_at = next(i for i in range(start + 1, len(lines)) if lines[i].strip() == "run: |")
    key_indent = len(lines[run_at]) - len(lines[run_at].lstrip())
    body = []
    for line in lines[run_at + 1:]:
        if line.strip() and len(line) - len(line.lstrip()) <= key_indent:
            break
        body.append(line)
    indent = min(len(l) - len(l.lstrip()) for l in body if l.strip())
    return "\n".join(l[indent:] for l in body) + "\n"


class BuildSteps(unittest.TestCase):
    """reproducible-build.yml's two build steps, run as Actions runs them
    (`bash -e`), with the environment script and cross stubbed."""

    STEPS = ("Build in the first checkout", "Build in the second checkout")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "scripts").mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.calls = self.root / "cross-calls"
        cross = self.bin / "cross"
        cross.write_text(
            '#!/bin/sh\nprintf "%s|%s\\n" "$SOURCE_DATE_EPOCH" "$RUSTFLAGS" >> "$CALLS"\n'
            'mkdir -p "target/$TARGET/release" && echo bin > "target/$TARGET/release/pixel"\n'
        )
        cross.chmod(0o755)

    def tearDown(self):
        self.tmp.cleanup()

    def stub_env_script(self, body: str) -> None:
        script = self.root / "scripts" / "release-build-env.sh"
        script.write_text("#!/bin/sh\n" + body)
        script.chmod(0o755)

    def run_step(self, step: str) -> subprocess.CompletedProcess:
        env = dict(
            os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", CALLS=str(self.calls),
            TARGET="x86_64-unknown-linux-musl", RUNNER_TEMP=str(self.root),
        )
        env.pop("RUSTFLAGS", None)
        env.pop("SOURCE_DATE_EPOCH", None)
        return subprocess.run(
            ["bash", "-e", "-c", run_block("reproducible-build.yml", step)],
            cwd=self.root, env=env, capture_output=True, text=True,
        )

    def test_failing_environment_script_stops_the_build(self):
        # `eval "$(script)"` would eval an empty string, succeed, and build
        # both sides without the environment: equal, and proving nothing.
        self.stub_env_script("echo broken >&2\nexit 1\n")
        for step in self.STEPS:
            with self.subTest(step=step):
                res = self.run_step(step)
                self.assertNotEqual(res.returncode, 0)
                self.assertFalse(self.calls.exists(), "cross ran without the environment")

    def test_environment_reaches_cross(self):
        self.stub_env_script("printf \"export SOURCE_DATE_EPOCH='7'\\nexport RUSTFLAGS='-x'\\n\"\n")
        for step in self.STEPS:
            with self.subTest(step=step):
                self.calls.unlink(missing_ok=True)
                res = self.run_step(step)
                self.assertEqual(res.returncode, 0, res.stderr)
                self.assertEqual(self.calls.read_text(), "7|-x\n")


class Wiring(unittest.TestCase):
    def test_cross_forwards_source_date_epoch(self):
        text = (ROOT / "Cross.toml").read_text()
        self.assertRegex(text, r'(?m)^\[build\.env\]\s*\npassthrough = \[[^\]]*"SOURCE_DATE_EPOCH"')

    def test_environment_is_set_before_cache_and_build(self):
        for workflow, build in (
            ("release-build.yml", "cross build --release"),
            ("cross-build.yml", "cross build --profile"),
            ("reproducible-build.yml", "cross build --release"),
        ):
            with self.subTest(workflow=workflow):
                parts = steps(workflow)
                env_at = index_of(parts, "scripts/release-build-env.sh")
                self.assertLess(env_at, index_of(parts, build))
                if any("Swatinem/rust-cache" in p for p in parts):
                    # rust-cache hashes RUSTFLAGS into its key when it restores.
                    self.assertLess(env_at, index_of(parts, "Swatinem/rust-cache"))

    def test_macos_release_build_gets_the_environment_too(self):
        parts = steps("release-build.yml")
        env_step = parts[index_of(parts, "scripts/release-build-env.sh")]
        self.assertNotIn("runner.os", env_step)

    def test_same_cross_version_everywhere(self):
        pins = {
            w: re.findall(r"tool: (cross\S*)", (WORKFLOWS / w).read_text())
            for w in ("release-build.yml", "cross-build.yml", "reproducible-build.yml")
        }
        versions = {p for found in pins.values() for p in found}
        self.assertEqual(len(versions), 1, pins)
        self.assertRegex(versions.pop(), r"^cross@\d+\.\d+\.\d+$")

    def test_reproducible_build_uses_the_release_command(self):
        release = (WORKFLOWS / "release-build.yml").read_text()
        cmd = re.search(r"run: (cross build --release .*)", release).group(1)
        matrix = re.search(
            r"- target: x86_64-unknown-linux-musl\n\s+os: \S+\n\s+features: (.*)", release
        ).group(1)
        expected = (
            cmd.replace("${{ matrix.features }}", matrix)
            .replace("${{ matrix.target }}", '"$TARGET"')
        )
        repro = (WORKFLOWS / "reproducible-build.yml").read_text()
        builds = re.findall(r"^\s*(cross build .*)$", repro, re.M)
        self.assertEqual(builds, [expected, expected])
        self.assertIn("TARGET: x86_64-unknown-linux-musl", repro)


if __name__ == "__main__":
    unittest.main()
