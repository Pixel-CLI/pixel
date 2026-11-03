---
paths:
  - "scripts/dev/pr-swarm.sh"
  - ".claude/settings.json"
---

# One rmux Pane Per Open-PR Worktree

Loaded when `scripts/dev/pr-swarm.sh` or the Claude settings are in play. This
This page **documents** the swarm; it does not register its hook. To enable
automatic startup, add the repository's `scripts/dev/pr-swarm.sh hook-session-start`
command to the user's Claude `SessionStart` configuration, such as
`~/.claude/settings.json`. Do not commit `.claude/settings.json`; it is
per-worktree, tool-local configuration.

## The tool

`scripts/dev/pr-swarm.sh` keeps one rmux pane per open pull request, each pane a
`claude` session sitting in that PR's worktree. Session `pr-swarm`, single
window `swarm`, `tiled` layout. A pane is titled `PR#<N> <branch>` and the
session inside it is named `claude -n pr-<N>-<branch-slug>`. The PR number
between `PR#` and the space is the identity key; no part of the diff algorithm
keys on the branch.

| Subcommand | Behaviour |
| --- | --- |
| `reconcile [--wait N\|--no-wait] [--dry-run]` | desired set (open PRs) − actual set (panes titled `PR#<N>`) → create, retitle in place, or tear down. Default `--wait 30`. |
| `status` | read-only table: PR, branch, worktree path, pane id, agent name, and the action `reconcile` would take. Takes no lock. |
| `up <PR> [--worktree]` | explicit opt-in: open the pane; `--worktree` materialises `~/Documents/pixel-pr-<N>` on `pr/<N>` from `origin/<headRefName>` first. |
| `down <PR> [--force]` | close that one pane, run the teardown rails, report each refusal. |
| `watch` | the detached loop `hook-session-start` forks: `reconcile` every `PIXEL_PR_SWARM_INTERVAL` (300 s), taking the lock per tick. |
| `hook-session-start` | the `SessionStart` entry point: fork `watch` and exit 0 on every path. |

Env overrides: `PIXEL_PR_SWARM_SESSION` (`pr-swarm`), `PIXEL_PR_SWARM_MAX_PANES`
(`6`), `PIXEL_PR_SWARM_TEARDOWN` (`safe`), `PIXEL_PR_SWARM_INTERVAL` (`300`),
`PIXEL_RMUX_BIN`, `PIXEL_CLAUDE_BIN`; the script also honours
`PIXEL_PR_SWARM_REPO`, `PIXEL_PR_SWARM_CACHE` and `PIXEL_PR_SWARM_STATE_DIR`.
Past `MAX_PANES` nothing further is created; the PR stays visible in `status`
with a skip line.

## The trigger is a SessionStart watcher, not launchd

The hook spawns a detached `watch` loop that inherits the Claude session's
macOS TCC grant, so `~/Documents` and gh's keyring auth are both reachable. A
`LaunchAgent` was tried and retired: a `gui/$UID` job ran but macOS TCC
answered `Operation not permitted` for every path under `~/Documents`
(verified), so it could not read the repo at all. The watcher re-arms on each
session start and dies on reboot or logout — enough, since it exists to keep
the swarm current while the user works. Every failure path exits 0: a session
start is never rejected.

## The rails around teardown

The pane close is cheap and reversible; the worktree removal is neither. So
the pane always closes on **merge**, and the worktree is removed only when
**every** rail holds. A refusal is a log line and a retitle
(`PR#435 feat/x [merged, worktree kept: dirty]`), never a non-zero exit, and
`--force` is never passed at any setting.

- **Never `$REPO` itself**, and never a path inside it.
- **Never the shared dependency cache** `~/Documents/pixel-integration`, and
  never a `pixel-mutants-preflight.*` scratch tree.
- **Clean and fully pushed.** `git status --porcelain` empty and no commits
  ahead of the branch's upstream; local commits the PR never saw are work the
  user would lose.
- **`CLOSED` unmerged never tears down.** `project-task.md` returns the board
  item to Todo, so the work may resume; killing it would destroy live work.
- **A PR with no worktree is skipped, never guessed**, and a failed or empty
  `gh pr list` is never read as "every PR closed" — teardown is driven only by
  a per-PR `gh pr view` that succeeded.

Teardown dial: `PIXEL_PR_SWARM_TEARDOWN=off` (close the pane, remove nothing)
/ `safe` (default: close pane, remove worktree when rails pass, keep branch) /
`aggressive` (also `--delete-branch` on the local ref). Branch refs always
survive under `off` and `safe`; `down --delete-branch` is the explicit
opt-in, separately from the env var.

## State

State lives under `${XDG_STATE_HOME:-$HOME/.local/state}/pixel/pr-swarm/`:
`reconcile.log`, `lock/`, `resolve.tsv` (hand-edited PR→worktree overrides,
shipped empty), `last-run.jsonl` and `watch.pid`. The last is the `watch`
loop's own claim, written with `set -C` so two session starts cannot both
win it; a pidfile whose pid is dead is reclaimed, and `hook-session-start`
reads it only as a shortcut — the claim is the authority. The **pane set
itself is derived from
rmux pane titles**, not from these files, so a lost sidecar cannot drift from
reality. The reconciler never starts an rmux server that is not already
running — it logs `rmux server unavailable; nothing to reconcile` and exits 0.

## What you never do

- **Never `rmux claude`** in a pane's command. It passes
  `--teammate-mode tmux` and opens its own attached session; a swarm pane runs
  plain `claude -n`.
- **Never key the diff on the branch.** Only `PR#<N>` identifies a pane, so a
  rename retitles in place instead of spawning a second pane.
- **Never name a variable `RMUX` or `TMUX` in the script.** `RMUX` is rmux's
  own socket spec — the `$TMUX` protocol variable under another name — and a
  local assignment shadows the inherited value for every child process. The
  binary path landed in it once and every call died with `i/o error: Socket
  operation on non-socket (os error 38)`, which the script reported as "rmux
  server unavailable". The binary lives in `RMUX_BIN`.
- **Never edit the swarm by hand and expect it to stick.** The next
  `reconcile` recomputes the pane set from the titles and the open PRs.

## Why this is a rule

Without it the wiring is invisible: a contributor sees panes appear and cannot
tell which script created them, what protects a merged PR's worktree from a
`git worktree remove`, or why launchd is not the trigger. The cost is one
script, one hook entry and this page; the cost of skipping it is a swarm that
tears down work it should have kept.
