#!/usr/bin/env python3
# SPDX-FileCopyrightText: The Pixel contributors
# SPDX-License-Identifier: MIT

"""Contract of scripts/pr-swarm.sh: what one reconcile does, and what it must not.

Every case runs the real script with three stub binaries on PATH -- `rmux`
(stateful: it answers find-panes from a JSON state file and records every call,
so a second run over the same state can be asserted to issue *nothing*), `gh`
(reads `$STUB_GH_DIR/prs.json` and `$STUB_GH_DIR/pr-<N>.json`, and can be made
to fail), and `claude` (records its argv, exits 0) -- because the contract is
what the reconciler decides from those answers, not what rmux does with a pane.

git is real. A mocked git would prove nothing about `git worktree remove`
refusing a dirty tree, which is the whole reason the rails exist, so every case
builds a throwaway repository and adds worktrees to it the way clean.sh's
contract test does.

The two assertions stubbing cannot make -- that a pane created this way really
shows `claude -n <name>` in its header bar, and that `select-pane -T` survives
Claude (it does not set the terminal title) -- were made once by hand against
an isolated rmux server; see the plan's probe A. The case that replaces them
here is the argv handed to `claude`, which is what the name derives from.
"""

import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import time
import unittest

SWARM = Path(__file__).with_name("pr-swarm.sh")
MARKER = "pr-swarm-contract"

# Every rmux subcommand that changes a pane or a layout. A reconcile over
# unchanged state must issue none of them; `find-panes`, `has-session` and
# `display-message` are reads and are expected.
MUTATING = {"new-session", "split-window", "kill-pane", "select-pane", "select-layout"}

AGENT_NAME_GRAMMAR = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")

STUB_RMUX = '''\
#!/usr/bin/env python3
"""stub rmux: a state file of panes, and a log of every call."""
import json, os, sys

args = list(sys.argv[1:])
with open(os.environ["STUB_RMUX_LOG"], "a") as fh:
    fh.write("\\t".join(args) + "\\n")

# rmux's own socket spec, as this process sees it. `RMUX` is a protocol
# variable the caller inherits, not something a caller may repurpose.
if os.environ.get("STUB_RMUX_SOCKET_ENV"):
    with open(os.environ["STUB_RMUX_SOCKET_ENV"], "a") as fh:
        fh.write(os.environ.get("RMUX", "<unset>") + "\\n")

# global flags before the command
while args and args[0].startswith("-"):
    a = args.pop(0)
    if a in ("-S", "-f", "-L") and args:
        args.pop(0)
cmd = args.pop(0) if args else ""

if cmd in [c for c in os.environ.get("STUB_RMUX_FAIL", "").split(",") if c]:
    sys.exit(1)

STATE = os.environ["STUB_RMUX_STATE"]


def load():
    try:
        with open(STATE) as fh:
            return json.load(fh)
    except Exception:
        return {"next": 1, "panes": []}


def save(doc):
    with open(STATE + ".tmp", "w") as fh:
        json.dump(doc, fh)
    os.replace(STATE + ".tmp", STATE)


TAKES_VALUE = ("-t", "-c", "-F", "-s", "-n", "-x", "-y", "-T", "-e", "-l",
               "--title-prefix", "--title", "--cwd", "--current-command")


def parse(rest):
    flags, pos, i = {}, [], 0
    while i < len(rest):
        a = rest[i]
        if a in TAKES_VALUE:
            flags[a] = rest[i + 1] if i + 1 < len(rest) else ""
            i += 2
        elif a.startswith("-"):
            i += 1
        else:
            pos.append(a)
            i += 1
    return flags, pos


flags, pos = parse(args)

if cmd == "find-panes":
    prefix = flags.get("--title-prefix")
    doc = load()
    panes = [p for p in doc["panes"] if prefix is None or p["title"].startswith(prefix)]
    print(json.dumps({"ok": True, "panes": panes, "schema_version": 1}))
elif cmd == "has-session":
    name = flags.get("-t", "")
    sys.exit(0 if any(p["session_name"] == name for p in load()["panes"]) else 1)
elif cmd in ("new-session", "split-window"):
    doc = load()
    session = flags.get("-s") if cmd == "new-session" else flags.get("-t", "").split(":")[0]
    window = "0"
    index = sum(1 for p in doc["panes"] if p["session_name"] == session)
    pane = {
        "pane_id": "%%%d" % doc["next"],
        "session_name": session,
        "window_index": window,
        "pane_index": str(index),
        "cwd": flags.get("-c", ""),
        "title": "",
        "current_command": "2.1.285",
    }
    doc["next"] += 1
    doc["panes"].append(pane)
    save(doc)
    if "-P" in args:
        print(pane["pane_id"])
elif cmd == "kill-pane":
    doc = load()
    doc["panes"] = [p for p in doc["panes"] if p["pane_id"] != flags.get("-t")]
    save(doc)
elif cmd == "select-pane":
    doc = load()
    for p in doc["panes"]:
        if p["pane_id"] == flags.get("-t"):
            p["title"] = flags.get("-T", "")
    save(doc)
elif cmd == "select-layout":
    pass
sys.exit(0)
'''

STUB_GH = '''\
#!/bin/sh
# stub gh: records the call, answers pr list and pr view from $STUB_GH_DIR.
# The whole log block runs in a subshell: a bare { } group would shift the
# positional parameters this very dispatch reads below.
( printf '%s' "$1"; shift; for a in "$@"; do printf '\\t%s' "$a"; done; printf '\\n' ) >> "$GH_LOG"
case "$1" in
  pr)
    case "$2" in
      list)
        if [ -f "$STUB_GH_DIR/list-exit" ]; then exit "$(cat "$STUB_GH_DIR/list-exit")"; fi
        cat "$STUB_GH_DIR/prs.json" ;;
      view)
        if [ -f "$STUB_GH_DIR/pr-$3.json" ]; then cat "$STUB_GH_DIR/pr-$3.json"
        else printf '%s' '{"state":""}'; fi ;;
    esac ;;
esac
exit 0
'''

STUB_CLAUDE = '''\
#!/bin/sh
printf '%s\\n' "$*" >> "$CLAUDE_LOG"
exit 0
'''


def write_stub(path: Path, body: str) -> None:
    path.write_text(body)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


class PrSwarmContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix=MARKER + "-")
        self.addCleanup(self.tmp.cleanup)
        # macOS hides its temp dir behind a /var -> /private/var symlink, and
        # both git and the script compare paths those two spellings of.
        self.root = Path(self.tmp.name).resolve()
        self.repo = self.root / "repo"
        self.cache = self.root / "pixel-integration"
        self.state = self.root / "state"
        for d in (self.repo, self.root / "home", self.root / "tmp", self.state):
            d.mkdir(parents=True)

        (self.repo / "src").mkdir()
        (self.repo / "src/lib.rs").write_text("pub fn a() {}\n")
        self.git("init", "-q", "-b", "main")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        # A remote-tracking ref, so the "no unpushed work" rail has something
        # to compare against: the fixture is offline, not a fork.
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")

        self.bin = self.root / "bin"
        self.bin.mkdir()
        write_stub(self.bin / "rmux", STUB_RMUX)
        write_stub(self.bin / "gh", STUB_GH)
        write_stub(self.bin / "claude", STUB_CLAUDE)

        self.gh_dir = self.root / "gh"
        self.gh_dir.mkdir()
        (self.gh_dir / "prs.json").write_text("[]")
        self.rmux_log = self.root / "rmux.log"
        self.rmux_state = self.root / "rmux-state.json"
        self.gh_log = self.root / "gh.log"
        self.claude_log = self.root / "claude.log"

        # A pid that is certainly gone: reaped by wait(), and asserted dead in
        # the one case that depends on it.
        reaped = subprocess.Popen(["/usr/bin/true"])
        reaped.wait()
        self.dead_pid = reaped.pid

        self.env = {
            **os.environ,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(self.root / "home"),
            "TMPDIR": str(self.root / "tmp"),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "PIXEL_PR_SWARM_REPO": str(self.repo),
            "PIXEL_PR_SWARM_CACHE": str(self.cache),
            "PIXEL_PR_SWARM_STATE_DIR": str(self.state),
            "PIXEL_RMUX_BIN": str(self.bin / "rmux"),
            "PIXEL_CLAUDE_BIN": str(self.bin / "claude"),
            "STUB_RMUX_LOG": str(self.rmux_log),
            "STUB_RMUX_STATE": str(self.rmux_state),
            "GH_LOG": str(self.gh_log),
            "STUB_GH_DIR": str(self.gh_dir),
            "CLAUDE_LOG": str(self.claude_log),
        }

    # ── fixtures ───────────────────────────────────────────────────────────

    def git(self, *args, at=None):
        return subprocess.run(
            ["git", "-C", str(at or self.repo), *args], check=True, capture_output=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
                 "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
                 "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"},
        )

    def add_worktree(self, name: str, branch: str, at: Path | None = None) -> Path:
        path = at if at is not None else self.root / name
        self.git("worktree", "add", "-q", str(path), "-b", branch, "main")
        return path

    def write_prs(self, prs: list[dict]) -> None:
        (self.gh_dir / "prs.json").write_text(json.dumps(prs))

    def pr(self, number: int, branch: str, oid: str = "0" * 12) -> dict:
        return {"number": number, "headRefName": branch, "headRefOid": oid,
                "isCrossRepository": False}

    def write_pr_state(self, number: int, state: str) -> None:
        (self.gh_dir / f"pr-{number}.json").write_text(
            json.dumps({"state": state, "mergedAt": None}))

    def seed_panes(self, panes: list[tuple]) -> None:
        """panes: (PR number, branch, cwd) -- as a previous reconcile left them."""
        doc = {"next": len(panes) + 1, "panes": []}
        for i, (num, branch, cwd) in enumerate(panes):
            doc["panes"].append({
                "pane_id": f"%{i}", "session_name": "pr-swarm",
                "window_index": "0", "pane_index": str(i),
                "cwd": str(cwd), "title": f"PR#{num} {branch}",
                "current_command": "2.1.285",
            })
        self.rmux_state.write_text(json.dumps(doc))

    # ── runners ────────────────────────────────────────────────────────────

    def run_swarm(self, *args: str, **env_over) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SWARM), *args],
            env={**self.env, **env_over},
            capture_output=True, text=True, cwd=str(self.repo), timeout=120,
        )

    def rmux_calls(self) -> list[list[str]]:
        if not self.rmux_log.exists():
            return []
        return [line.split("\t") for line in self.rmux_log.read_text().splitlines() if line]

    def mutations_since(self, mark: int) -> list[list[str]]:
        return [c for c in self.rmux_calls()[mark:] if c and c[0] in MUTATING]

    def commands_since(self, mark: int, name: str) -> list[list[str]]:
        return [c for c in self.rmux_calls()[mark:] if c and c[0] == name]

    def gh_calls(self) -> list[list[str]]:
        if not self.gh_log.exists():
            return []
        return [line.split("\t") for line in self.gh_log.read_text().splitlines() if line]

    def log_lines(self) -> list[str]:
        path = self.state / "reconcile.log"
        return path.read_text().splitlines() if path.exists() else []

    def pane_titles(self) -> list[str]:
        doc = json.loads(self.rmux_state.read_text()) if self.rmux_state.exists() else {"panes": []}
        return [p["title"] for p in doc["panes"]]

    def pane_ids(self) -> list[str]:
        doc = json.loads(self.rmux_state.read_text()) if self.rmux_state.exists() else {"panes": []}
        return [p["pane_id"] for p in doc["panes"]]

    def worktree_listing(self) -> str:
        return subprocess.run(["git", "-C", str(self.repo), "worktree", "list", "--porcelain"],
                              capture_output=True, text=True).stdout

    def claude_calls(self) -> list[str]:
        if not self.claude_log.exists():
            return []
        return self.claude_log.read_text().splitlines()

    def assert_command_absent(self, mark: int, name: str):
        self.assertEqual(self.commands_since(mark, name), [], f"{name} was issued")

    # ── 1. idempotency ─────────────────────────────────────────────────────

    def test_reconcile_twice_on_unchanged_state_issues_no_mutations(self):
        """The property that makes the hook and the watch timer safe to overlap.

        Everything else in this file is a decision made once; this is the one
        that says the reconciler is a function of the state it reads, so a
        second caller arriving mid-tick costs nothing and changes nothing.
        """
        a = self.add_worktree("wt-a", "feat/a")
        b = self.add_worktree("wt-b", "feat/b")
        self.write_prs([self.pr(1, "feat/a"), self.pr(2, "feat/b")])

        first = self.run_swarm("reconcile", "--no-wait")
        self.assertEqual(first.returncode, 0, first.stderr)
        # the first run really does create, or the case would be vacuous
        self.assertEqual(len(self.commands_since(0, "new-session")), 1)
        self.assertEqual(len(self.commands_since(0, "split-window")), 1)
        self.assertEqual(sorted(self.pane_titles()), ["PR#1 feat/a", "PR#2 feat/b"])
        # The pane command itself: rmux is stubbed, so the argv it was handed
        # is the contract -- retitle first (the title survives Claude, which
        # does not set one), then the named session.
        commands = [self.commands_since(0, "new-session")[0][-1],
                    self.commands_since(0, "split-window")[0][-1]]
        for cmd, title, name in zip(commands,
                                    ["PR#1 feat/a", "PR#2 feat/b"],
                                    ["pr-1-feat-a", "pr-2-feat-b"]):
            self.assertIn(f"select-pane -T '{title}'", cmd)
            self.assertIn(f" -n '{name}'", cmd)
            self.assertIn(str(self.bin / "claude"), cmd)

        mark = len(self.rmux_calls())
        second = self.run_swarm("reconcile", "--no-wait")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(self.mutations_since(mark), [])

    # ── 2-3. gh cannot be read as "every PR was closed" ────────────────────

    def test_gh_outage_does_not_tear_anything_down(self):
        """The catastrophic failure: a failed gh call is not an empty PR set."""
        wt = self.add_worktree("wt-a", "feat/a")
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([self.pr(1, "feat/a")])
        (self.gh_dir / "list-exit").write_text("1")

        marker = len(self.log_lines())
        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "kill-pane")
        self.assertNotIn("--force", r.stdout + r.stderr)
        self.assertTrue(wt.exists())
        self.assertIn(str(wt), self.worktree_listing())
        self.assertEqual(len(self.log_lines()) - marker, 1)
        self.assertIn("leaving panes alone", self.log_lines()[-1])
        # it never even asked per-PR
        self.assertEqual([c[1] for c in self.gh_calls() if c[0] == "pr"], ["list"])

    def test_gh_list_returning_empty_never_tears_down(self):
        """Same shape as the outage, and pinned separately: `[]` is not proof.

        Teardown is driven by the per-PR `gh pr view`, so a PR missing from a
        successful empty list still has to be asked about individually.
        """
        wt = self.add_worktree("wt-a", "feat/a")
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([])
        self.write_pr_state(1, "OPEN")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "kill-pane")
        self.assertTrue(wt.exists())
        views = [c[2] for c in self.gh_calls() if c[0] == "pr" and c[1] == "view"]
        self.assertEqual(views, ["1"])

    def test_a_fork_pr_is_never_adopted(self):
        """A fork PR's branch name belongs to a stranger.

        Matching it by name against a local worktree would put their commits in
        a pane whose cwd is this repository's checkout, so a cross-repository
        PR is dropped from the desired set before resolution can see it.
        """
        self.add_worktree("wt-a", "feat/a")
        self.write_prs([{"number": 9, "headRefName": "feat/a",
                         "headRefOid": "0" * 12, "isCrossRepository": True}])

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "new-session")
        self.assert_command_absent(0, "split-window")
        self.assertEqual(self.pane_ids(), [])
        self.assertNotIn("PR#9", "\n".join(self.log_lines()))

    # ── 4-8. teardown ──────────────────────────────────────────────────────

    def test_merged_pr_closes_the_pane_and_removes_the_worktree(self):
        wt = self.add_worktree("wt-a", "feat/a")
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertNotIn("--force", r.stdout + r.stderr)
        self.assertFalse(wt.exists())
        self.assertNotIn(str(wt), self.worktree_listing())
        self.assertEqual(self.pane_ids(), [])

    def test_merged_pr_with_a_dirty_worktree_closes_the_pane_and_keeps_it(self):
        """The pane closes -- that half is cheap and reversible.

        The worktree does not: untracked files are exactly what an agent
        leaves behind, and `--force` is never passed at any setting.
        """
        wt = self.add_worktree("wt-a", "feat/a")
        (wt / "scratch.txt").write_text("work in progress\n")
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertTrue(wt.exists())
        self.assertTrue((wt / "scratch.txt").exists())
        self.assertIn(str(wt), self.worktree_listing())
        self.assertIn("1 modified or untracked files", "\n".join(self.log_lines()))

    def test_merged_pr_never_removes_the_main_checkout(self):
        self.seed_panes([(1, "feat/a", self.repo)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertTrue(self.repo.exists())
        self.assertIn("main checkout", "\n".join(self.log_lines()))

    def test_merged_pr_never_removes_the_shared_dependency_cache(self):
        """pixel-integration is build output, not a checkout.

        It is not a worktree by design, so the fixture makes it one: the rail
        has to hold against the state it exists to refuse, not only against
        the state that cannot occur.
        """
        self.git("worktree", "add", "-q", str(self.cache), "-b", "integration/x", "main")
        self.seed_panes([(1, "feat/a", self.cache)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertTrue(self.cache.exists())
        self.assertIn(str(self.cache), self.worktree_listing())
        self.assertIn("shared dependency cache", "\n".join(self.log_lines()))

    def test_merged_pr_never_removes_a_mutants_preflight_scratch_tree(self):
        """Removing one mid-campaign corrupts a run that is still in flight."""
        wt = self.add_worktree("pixel-mutants-preflight.abc", "scratch/x",
                               at=self.root / "tmp" / "pixel-mutants-preflight.abc")
        self.seed_panes([(1, "scratch/x", wt)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertTrue(wt.exists())
        self.assertIn("mutants preflight scratch tree", "\n".join(self.log_lines()))

    def test_merged_pr_with_unpushed_commits_keeps_its_worktree(self):
        """A clean tree is not enough: clean says nothing about where it went.

        Commits no remote carries exist only here, so the pane closes and the
        directory stays, whether or not someone remembers to look.
        """
        wt = self.add_worktree("wt-a", "feat/a")
        (wt / "src/lib.rs").write_text("pub fn b() {}\n")
        self.git("commit", "-aqm", "not on any remote", at=wt)
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([])
        self.write_pr_state(1, "MERGED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "kill-pane")), 1)
        self.assertTrue(wt.exists())
        self.assertIn(str(wt), self.worktree_listing())
        self.assertIn("commits not on any remote", "\n".join(self.log_lines()))

    def test_closed_unmerged_pr_keeps_its_pane_and_worktree(self):
        """A closed-unmerged PR returns its board item to Todo.

        The work may resume, so the worktree is live work and the pane is the
        only thing pointing at it.
        """
        wt = self.add_worktree("wt-a", "feat/a")
        self.seed_panes([(1, "feat/a", wt)])
        self.write_prs([])
        self.write_pr_state(1, "CLOSED")

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "kill-pane")
        self.assertTrue(wt.exists())
        self.assertIn(str(wt), self.worktree_listing())
        self.assertIn("closed unmerged", "\n".join(self.log_lines()))

    # ── 9. an open PR with no worktree ─────────────────────────────────────

    def test_an_open_pr_without_a_worktree_creates_nothing(self):
        """Creating one needs a base decision the reconciler must not make.

        The branch may not exist locally at all, and the only worktree-creation
        idiom here is the dev-stack union; `up <PR> --worktree` is the opt-in.
        """
        self.write_prs([self.pr(3, "feat/nowhere", oid="c" * 12)])

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "new-session")
        self.assert_command_absent(0, "split-window")
        self.assertEqual(self.pane_ids(), [])
        self.assertIn("unresolved", "\n".join(self.log_lines()))

    # ── 10. the naming decision ────────────────────────────────────────────

    def test_a_branch_rename_retitles_in_place(self):
        """No part of the diff keys on the branch.

        The number anchors identity; a rename changes the readable tail of the
        title and nothing else, so the pane is not restarted and no second pane
        is spawned.
        """
        wt = self.add_worktree("wt-rebase", "plan-prereqs-rebase")
        self.seed_panes([(434, "plan-prereqs-spec", wt)])
        self.write_prs([self.pr(434, "plan-prereqs-rebase")])

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        retitles = self.commands_since(0, "select-pane")
        self.assertEqual(len(retitles), 1, retitles)
        self.assertIn("PR#434 plan-prereqs-rebase", retitles[0])
        self.assertEqual(retitles[0].count("%0"), 1)
        self.assert_command_absent(0, "split-window")
        self.assert_command_absent(0, "new-session")
        self.assertEqual(self.pane_ids(), ["%0"])
        self.assertEqual(self.pane_titles(), ["PR#434 plan-prereqs-rebase"])

    # ── 11-13. lock and rmux availability ──────────────────────────────────

    def test_a_held_lock_is_a_silent_no_op(self):
        """--no-wait is the hook path: it must return without writing anything.

        A log line per session start would fill the file with nothing, and a
        non-zero exit would reject a session start.
        """
        lock = self.state / "lock"
        lock.mkdir()
        (lock / "pid").write_text(str(os.getpid()))     # live: this test process
        mark = len(self.log_lines())

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.log_lines()[mark:], [])
        self.assertEqual(self.rmux_calls(), [])
        self.assertTrue(lock.exists(), "a live lock must not be stolen")

    def test_a_stale_lock_is_reclaimed(self):
        lock = self.state / "lock"
        lock.mkdir()
        (lock / "pid").write_text(str(self.dead_pid))
        self.assertEqual(
            subprocess.run(["/bin/kill", "-0", str(self.dead_pid)],
                           capture_output=True).returncode != 0, True,
            "the fixture pid must be dead for this case to mean anything")
        wt = self.add_worktree("wt-a", "feat/a")
        self.write_prs([self.pr(1, "feat/a")])

        r = self.run_swarm("reconcile", "--no-wait")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "new-session")), 1)
        self.assertEqual(self.pane_titles(), ["PR#1 feat/a"])
        self.assertTrue(wt.exists())

    def test_rmux_unavailable_creates_nothing(self):
        """A user who is not running rmux is not an error, and no server is
        started on their behalf."""
        self.add_worktree("wt-a", "feat/a")
        self.write_prs([self.pr(1, "feat/a")])

        r = self.run_swarm("reconcile", "--no-wait", STUB_RMUX_FAIL="find-panes")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assert_command_absent(0, "new-session")
        self.assert_command_absent(0, "split-window")
        self.assertIn("rmux server unavailable", "\n".join(self.log_lines()))

    # ── the watcher (extra to the plan's 15) ───────────────────────────────
    # The hook's job is the decided design's trigger, and it is the one part
    # with a real process to leave behind, so all three cases clean up after
    # themselves rather than trusting the temp dir's removal.

    def watch_pid(self) -> str:
        path = self.state / "watch.pid"
        return path.read_text().strip() if path.exists() else ""

    def await_path(self, path: Path, timeout: float = 15.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists():
                return True
            time.sleep(0.05)
        return False

    def await_watch_pid(self, timeout: float = 15.0) -> str:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            pid = self.watch_pid()
            if pid:
                return pid
            time.sleep(0.05)
        return ""

    def kill_watch(self) -> None:
        """Stop a spawned watcher and leave no process behind for the next run."""
        pid = self.watch_pid()
        if not pid:
            return
        subprocess.run(["/bin/kill", "-TERM", pid], capture_output=True)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if subprocess.run(["/bin/kill", "-0", pid],
                              capture_output=True).returncode != 0:
                return
            time.sleep(0.05)

    def stop(self, proc: subprocess.Popen) -> None:
        """Never leave a looping watcher behind when a case fails midway."""
        if proc.poll() is not None:
            return
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)

    def test_hook_session_start_spawns_the_watch_loop_and_returns_at_once(self):
        """The hook is on the session-start path: it is never waited on.

        It spawns the interval loop, not one reconcile, or the swarm reconciles
        once per session and PIXEL_PR_SWARM_INTERVAL never fires at all.
        """
        self.addCleanup(self.kill_watch)

        started = time.monotonic()
        r = self.run_swarm("hook-session-start", PIXEL_PR_SWARM_INTERVAL="2")
        elapsed = time.monotonic() - started

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertLess(elapsed, 10, "the hook waited on the watcher")
        pid = self.await_watch_pid()
        self.assertTrue(pid, "the hook spawned no watcher")
        self.assertNotEqual(pid, str(os.getpid()))
        self.assertEqual(
            subprocess.run(["/bin/kill", "-0", pid], capture_output=True).returncode,
            0, "the pidfile named a process that is not running")
        self.assertIn("watch started (interval 2s",
                      "\n".join(self.log_lines()))
        # The interval really is the loop's: a first tick lands immediately, and
        # with nothing open it leaves only its machine-readable record.
        self.assertTrue(self.await_path(self.state / "last-run.jsonl"),
                        "the watcher never ran a tick")

    def test_a_second_hook_session_start_does_not_stack_a_watcher(self):
        """A restart, a second window, a double fire: one loop, not three.

        Stacked watchers reconcile the same panes several times a tick for the
        rest of the day, which is invisible until it is a rate limit.
        """
        self.addCleanup(self.kill_watch)

        self.run_swarm("hook-session-start", PIXEL_PR_SWARM_INTERVAL="2")
        first = self.await_watch_pid()
        self.assertTrue(first, "the first hook spawned no watcher")

        second = self.run_swarm("hook-session-start", PIXEL_PR_SWARM_INTERVAL="2")
        time.sleep(1.0)

        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(self.watch_pid(), first, "a second watcher took over")
        starts = [l for l in self.log_lines() if "watch started" in l]
        self.assertEqual(len(starts), 1, starts)

    def test_watch_refuses_a_second_instance_and_reclaims_a_stale_pidfile(self):
        """The pidfile is the authority; the hook's check is only a shortcut.

        Two hook processes can pass that check at the same moment, so the
        guarantee has to live in `watch` itself -- and a pidfile whose owner was
        killed, not closed, must not lock the swarm out of ever watching again.
        """
        (self.state / "watch.pid").write_text(f"{os.getpid()}\n")

        refused = self.run_swarm("watch", PIXEL_PR_SWARM_INTERVAL="2")

        self.assertEqual(refused.returncode, 0, refused.stderr)
        self.assertEqual(self.watch_pid(), str(os.getpid()),
                         "a live watcher's pidfile was overwritten")
        self.assertIn("already running", "\n".join(self.log_lines()))

        (self.state / "watch.pid").write_text(f"{self.dead_pid}\n")
        proc = subprocess.Popen(
            ["bash", str(SWARM), "watch"],
            env={**self.env, "PIXEL_PR_SWARM_INTERVAL": "2"},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self.addCleanup(self.stop, proc)

        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and self.watch_pid() != str(proc.pid):
            time.sleep(0.05)
        self.assertEqual(self.watch_pid(), str(proc.pid),
                         "a stale pidfile was not reclaimed")

        proc.terminate()
        proc.wait(timeout=15)
        self.assertEqual(self.watch_pid(), "",
                         "a killed watcher left its pidfile behind")

    # ── 14. MAX_PANES ──────────────────────────────────────────────────────

    def test_max_panes_creates_six_and_logs_the_skip(self):
        prs = []
        for n in range(1, 8):
            self.add_worktree(f"wt-{n}", f"feat/p{n}")
            prs.append(self.pr(n, f"feat/p{n}"))
        self.write_prs(prs)

        r = self.run_swarm("reconcile", "--no-wait", PIXEL_PR_SWARM_MAX_PANES="6")

        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.commands_since(0, "new-session")), 1)
        self.assertEqual(len(self.commands_since(0, "split-window")), 5)
        self.assertEqual(len(self.pane_ids()), 6)
        skips = [l for l in self.log_lines() if "skipped" in l]
        self.assertEqual(len(skips), 1, skips)
        self.assertIn("7 open PRs, MAX_PANES=6", skips[0])

    # ── 15. the agent name ─────────────────────────────────────────────────

    def test_agent_name_matches_the_session_name_grammar(self):
        """A Claude session name admits no dot; slugify keeps them for paths."""
        cases = [
            ("435", "feat/ai-cli-readify", "pr-435-feat-ai-cli-readify"),
            ("7", "release/v1.2.3", "pr-7-release-v1_2_3"),
            ("12", "a" * 120, "pr-12-" + "a" * 58),
            ("3", "feat/weird~chars", "pr-3-feat-weird-chars"),
        ]
        for num, branch, expected in cases:
            # Sourcing runs the dispatch table too, and a sourced script sees
            # the caller's positionals: clear them so it lands on `usage`.
            out = subprocess.run(
                ["bash", "-c",
                 'swarm="$1" num="$2" branch="$3"; set --; '
                 '. "$swarm" >/dev/null 2>&1; agent_name "$num" "$branch"',
                 "_", str(SWARM), num, branch],
                env=self.env, capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(out.returncode, 0, out.stderr)
            name = out.stdout.strip()
            self.assertEqual(name, expected)
            self.assertEqual(len(name), min(len(expected), 64))
            self.assertRegex(name, AGENT_NAME_GRAMMAR)

    # ── 22-23. the two env/rc traps ──

    def test_the_script_leaves_rmuxs_socket_pointer_alone(self):
        """`RMUX` is rmux's socket spec (tmux's `$TMUX`), not ours.

        A local `RMUX=<binary path>` shadows it, and rmux then opens the
        binary as a socket: `i/o error: Socket operation on non-socket
        (os error 38)` for every call, which surfaces as "rmux server
        unavailable". The inherited value must reach the child untouched.
        """
        self.add_worktree("wt435", "feat/x")
        self.write_prs([self.pr(435, "feat/x")])
        self.seed_panes([])
        sock = self.root / "rmux-env.log"
        inherited = "/tmp/rmux-501/rmux-claude-deadbeef,44448,0"
        r = self.run_swarm("status", STUB_RMUX_SOCKET_ENV=str(sock), RMUX=inherited)
        self.assertEqual(r.returncode, 0, r.stderr)
        seen = sock.read_text().splitlines()
        self.assertTrue(seen, "the stub rmux was never invoked")
        self.assertEqual([inherited] * len(seen), seen)

    def test_status_reports_a_gh_failure_only_when_gh_failed(self):
        """Success leaves `rc` at its initial 0, so the case needs a `0)` arm.

        `local rc=0` + `gather || rc=$?` means rc==0 on success; a case with
        no `0)` arm sends it to `*)` and reports a gh failure that never
        happened. Pinned in both directions: the success path prints the
        table, and a real gh failure still says so.
        """
        self.add_worktree("wt435", "feat/x")
        self.write_prs([self.pr(435, "feat/x")])
        self.seed_panes([])

        ok = self.run_swarm("status")
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertNotIn("gh pr list failed", ok.stderr)
        self.assertIn("BRANCH", ok.stdout)
        self.assertIn("#435", ok.stdout)
        self.assertIn("feat/x", ok.stdout)

        (self.gh_dir / "list-exit").write_text("1")
        broken = self.run_swarm("status")
        self.assertEqual(broken.returncode, 0, broken.stderr)
        self.assertIn("gh pr list failed", broken.stderr)

    # ── 24-26. CodeRabbit convergence ──

    def test_cmd_up_does_not_create_a_duplicate_pane_when_one_exists(self):
        """cmd_up must read existing panes before its pane_id_of guard.

        Without it, `up 435` after a restart where the pane is already in
        the rmux server would silently create a second one. Pinned by
        seeding the rmux state with the pane and asserting the rmux
        mutation log has no `new-session` for it.
        """
        wt = self.add_worktree("wt435", "feat/x")
        self.write_prs([self.pr(435, "feat/x")])
        # Seed the per-PR gh stub so cmd_up can read headRefName + headRefOid.
        (self.gh_dir / "pr-435.json").write_text(json.dumps({
            "headRefName": "feat/x", "headRefOid": "0" * 12,
        }))
        # Seed an existing pane for #435, owned by the same worktree.
        self.seed_panes([(435, "feat/x", str(wt))])

        before = len([c for c in self.rmux_calls() if c and c[0] == "new-session"])
        out = self.run_swarm("up", "435")
        self.assertEqual(out.returncode, 0, out.stderr)
        after = len([c for c in self.rmux_calls() if c and c[0] == "new-session"])
        self.assertEqual(after, before,
            "cmd_up created a duplicate pane for an existing PR")

    def test_cmd_down_refuses_to_touch_panes_for_non_merged_states(self):
        """cmd_down gates teardown on state=MERGED, not on !=OPEN.

        OPEN, CLOSED, and unknown states must all refuse without --force.
        MERGED must proceed. The reasoning: a pane's PR can flip through
        CLOSED on its way to MERGED, and the rails belong to MERGED.
        Pinned by exercising CLOSED and unknown: both must refuse, and
        only MERGED may remove.
        """
        wt = self.add_worktree("wt435", "feat/x")
        self.write_prs([])
        self.seed_panes([(435, "feat/x", str(wt))])

        # CLOSED: must refuse. The pre-fix gate only refused OPEN, so a
        # CLOSED pane would fall through to `kill-pane`.
        self.write_pr_state(435, "CLOSED")
        before = self._pane_count()
        r = self.run_swarm("down", "435")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("needs state=MERGED", r.stderr)
        self.assertEqual(self._pane_count(), before,
            "CLOSED pane was killed; the gate only blocks OPEN")

        # Unknown (state empty): must refuse.
        self.write_pr_state(435, "")
        r = self.run_swarm("down", "435")
        self.assertIn("needs state=MERGED", r.stderr)
        self.assertEqual(self._pane_count(), before)

        # MERGED: must proceed past the gate (rails may still refuse,
        # which is why this test only checks the gate).
        self.write_pr_state(435, "MERGED")
        r = self.run_swarm("down", "435")
        self.assertNotIn("needs state=MERGED", r.stderr)

    def _pane_count(self) -> int:
        if not self.rmux_state.exists():
            return 0
        return len(json.loads(self.rmux_state.read_text()).get("panes", []))

    def test_removal_refusal_protects_a_worktree_an_open_pr_still_needs(self):
        """Rail 8: refuse when DESIRED lists the owner for an open PR.

        A pane's PR can flip back to OPEN between `gh pr list` and the
        state view. The worktree is for the PR, not the pane.
        """
        wt = self.add_worktree("wt435", "feat/x")
        # A pane for #435 exists and the PR is MERGED -- so the reconcile
        # teardown path runs.
        self.seed_panes([(435, "feat/x", str(wt))])
        self.write_pr_state(435, "MERGED")
        # But the open-PR list has only #436, which also lives on this
        # worktree path (stacked-PR shape). `gh_open_prs` returns #436,
        # not #435, so the pane is eligible for teardown while DESIRED
        # still has the worktree path.
        self.write_prs([
            {"number": 436, "headRefName": "feat/x",
             "headRefOid": "0" * 12, "isCrossRepository": False},
        ])
        (self.gh_dir / "pr-436.json").write_text(json.dumps({
            "headRefName": "feat/x", "headRefOid": "0" * 12,
        }))

        before = self._pane_count()
        r = self.run_swarm("reconcile", "--no-wait")
        self.assertEqual(r.returncode, 0, r.stderr)
        log = self.log_lines()
        if not any("in use by an open PR" in line for line in log):
            print("=== reconcile log ===")
            for line in log: print(line)
            print("=== rmux log ===")
            for line in self.rmux_calls(): print(line)

        # The pane is gone (MERGED -> kill-pane ran), but the worktree
        # must remain in git. Without rail 8 the worktree would be removed
        # while PR #436 still claims it.
        out = subprocess.run(
            ["git", "-C", str(self.repo), "worktree", "list"],
            capture_output=True, text=True, check=True,
        )
        self.assertIn(str(wt), out.stdout,
            "worktree removed while an open PR still lists it in DESIRED")
        self.assertTrue(any("in use by an open PR" in line for line in log),
            f"rail 8 reason not logged; log was: {log!r}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
