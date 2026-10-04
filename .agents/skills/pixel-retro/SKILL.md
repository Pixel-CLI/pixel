---
name: pixel-retro
description: Mine the last 24 h (or a given window) of pixel usage — every repo's `.pixel/actions.jsonl` plus the agent transcripts indexed by `pixel recall` (claude, codex, pi…) — for frictions pixel actually caused, verify each one against the current binary and code, and propose ranked, evidence-backed improvements (bug fix, error message, flag, agent prompt, rule, doc); for the pixel repo itself, also measure each pull request from first edit to validated CI and where agents sat idle (`lead_time.py`). Suggests only; implements nothing without the user's pick. Use when the user says "/pixel-retro", "retro pixel", "qu'est-ce qui a coincé avec pixel", "améliorations pixel depuis les transcripts", "pixel friction", "où l'agent attend", "bottleneck", "lead time", or asks what to improve in pixel or its development workflow from recent sessions.
---

# Pixel retro

Turn what pixel did to agents over a recent window into a short list of
improvements, each anchored to something that happened. No evidence, no
suggestion: an idea you cannot point to in an action log or a transcript
turn belongs in a separate conversation.

Argument: a window (`24h` by default, `48h`, `7d`, an ISO date). Written
`$W` below.

## Step 0 — Fresh corpus, known baseline

```bash
pixel recall daemon status          # not running → the corpus is stale
pixel recall index --stats          # incremental, a few seconds
pixel recall status                 # last ingest must be minutes old, per agent
pixel --version                     # the binary the suggestions are judged against
pixel commit-history                # what main gained inside the window
pixel --metrics off doctor . --only install.agent-prompt --only install.pi-prompt
```

The last line says which prompt the agents in the transcripts were
reading (`--only` is newer than 0.4.0: on an older binary, run `pixel
doctor .` and read those two checks). A stale one — red, or the `note:
agent-prompt.md deployed by \`pixel install\` differs…` line every command
prints since #243 — means the misuses in the window may follow an older
release's command map: judge them against that prompt, not the current one.

Stop and say so if an agent's last ingest predates the window: a quiet
window on a stale corpus is not a quiet window. Semantic search may be off
(`semantic: no vectors yet`); `pixel recall ask` then degrades to lexical,
so prefer `search` with explicit regexes.

Read the ledger `~/.local/state/pixel-retro/seen.tsv` (columns:
`date  fingerprint  verdict  ref`; a missing file means an empty ledger).
A fingerprint is `<command>|<error normalised: paths, numbers, ids → _>`.
Anything already `fixed`, `wontfix`, `suggested` or `not-pixel` less than 7 days ago is
skipped unless its count grew.

## Step 1 — Collect the raw signal

Two sources, in this order: the action log is structured and exact, the
transcripts explain what the agent did about it.

**A. Action logs.** Every pixel invocation appends to
`<root>/.pixel/actions.jsonl` (`ts_ms`, `command`, `args`, `cwd`,
`outcome`, `error`, `duration_ms`, `serve`). The roots come from the sessions
themselves, so a repository outside the usual folders is not missed: take
every session `cwd` in the window, resolve it to its repository root, and
keep the roots that have a log. `pixel recall sessions` caps `--limit` at
200 and has no offset, so `roots.py` cuts the window into time slices and
halves any slice that comes back full; it dedupes by session id, includes
subagent sessions, and warns if 200 sessions overlap a single second. The `find` adds the worktrees and fixtures no
session ran in:

```bash
python3 .agents/skills/pixel-retro/roots.py $W
find ~/code /tmp/pxwt -maxdepth 4 -path '*/.pixel/actions.jsonl' -mtime -1 2>/dev/null   # -mtime -2 for 48h, and so on
```

then per root `pixel action-log --errors-only --json --limit 500 <root>`,
keeping entries whose `ts_ms` falls in the window. A read op that takes
seconds is a friction even when it succeeds; group the slow ones by how
they were served:

```bash
python3 .agents/skills/pixel-retro/slow.py $W <root>/.pixel/actions.jsonl ...   # --min-ms 5000 by default
```

It keeps read ops (`search-*`, `find-*`, `impact`, `who-calls`,
`call-path`, `scope-task`, `pack-context`, `status`, `repo-state`, and
`recall` only for `search`/`ask`/`context`/`show`/`sessions`/`status`: a slow
`recall index` is an ingest, not an answer) over the threshold, leaves the
test suite out (cwd under `crates/` or the temp dir, `/tmp/pxwt/` kept), and groups them
on (command, route, reason, dominant phase) with count, total, worst, and
the three worst `invocation_id`s. Each line's `serve` list (#241, #242)
records who answered each request and where its milliseconds went; name a
cause from it, never from the command alone:

| `serve` shows | Where the time went | Look at |
| --- | --- | --- |
| `daemon`, `probe_ms` dominant | the queue ahead: the daemon serves one request at a time and flushes its watcher batch first | a concurrent call in the same seconds (another agent, a hook), a large pending batch after a checkout (`daemon.rs` `flush_pending`) |
| `daemon`, `request_ms` dominant | the op itself, lazy graph or index work included | profile the op on that repo |
| `daemon_started`, `start_ms` dominant | a cold start: no daemon (30 min idle exit, reboot, upgrade) | how long `Service::open` takes there; retiring a stale daemon after an upgrade counts here too |
| `in_process` `start_timed_out`, `start_ms` ≈ 5 000 then `open_ms` | the start outlasted the 5 s wait and the CLI opened the index a second time, in process, next to the starting daemon | daemon startup order (`daemon::run` opens the `Service` before taking the lock); PE-01 in `docs/audit/pixel-retro-2026-09-23` |
| `in_process` `auto_start_disabled` / `no_daemon`, `open_ms` | every call opens the index itself, by configuration | `PIXEL_DAEMON_AUTO_START=0` or `--no-daemon` in that agent's setup: a config finding, not a pixel bug |
| `in_process` `newer_daemon` | two pixel binaries on one root, the older one declining the newer daemon | install drift (Step 3) |
| recall `daemon_absent` / `not_routed`, `open_ms` | the transcript catch-up and the model load, in process (`context` has no daemon route) | the recall daemon was not running (`pixel recall daemon status`) |
| recall `daemon`, `open_ms` dominant | `search` catches up in process before it asks the daemon | the catch-up itself, not the daemon |
| recall `daemon_error` | the daemon answered with an error and the query was redone in process | the error on the stderr of that call |
| `unattributed` | the line predates `serve` | nothing to conclude: say so, and do not read a warm replay under 2 s as "not reproduced" |

Drop the noise before counting (verified 2026-09):

- **pixel's own test suite.** `cargo test` spawns the CLI with `cwd` under
  `<pixel>/crates/…` or args under `/var/folders/…`, `$TMPDIR`, `/tmp/`, and
  those runs land in the real repo's `actions.jsonl` (37 of 40 errors in one
  sample were `check-release`/`classify` test cases). Exclude them from the
  counts; report the leak itself once as a finding while it lasts.
- **The stale-prompt note and a side build.** A `pixel-dev` never prints it.
  The managed `pixel` printing it on every call while `~/.claude/settings.json`
  runs `pixel-dev run-hook …` means a global `pixel-dev install` took the home
  install: report it once, with `pixel install` as the hand-back, not as drift.
- **Refusals that are the contract.** `fast-forward` refusing a non-ff,
  `commit` refusing a dirty or empty stage, a `--request-id` replay: an
  error is a friction only if the agent had to work around it (Step 2 says
  how to tell).

**B. Transcripts.** Regex over the corpus, newest first, always with
`--since $W`. Run each probe once across all repos, then narrow with
`--repo` where the hits cluster:

| Probe | Command |
| --- | --- |
| pixel call that failed | `pixel recall search '(error|Error|unexpected argument|unrecognized|panicked|exited with code [1-9])' --role tool --since $W` then keep hits whose preceding assistant turn runs `pixel` |
| agent bypassed pixel | `pixel recall search '⋮tool Bash \{"command":"[^"]*\b(rg|grep -r|git (log -S|blame|diff|status))\b' --role assistant --since $W` |
| retry loop | three or more `pixel search-*` / `find-*` calls on the same topic within a few turns of one session (`pixel recall show <ref> --turn N..M`) |
| empty or truncated answer | `pixel recall search '\b(unresolved|capped)\b' --role tool --since $W` |
| human complaint | `pixel recall search 'pixel' --role user --human-only --since $W` (`--human-only` alone keeps assistant and tool turns, it only drops injected user text), read for "marche pas", "lent", "pourquoi", "bug", "encore", "wrong" |
| harness or install drift | `pixel recall search '(doctor|install\.|daemon (lock|not running)|stale)' --role tool --since $W` |
| prompts older than the binary | `pixel recall search 'deployed by .pixel install. differs? from' --role tool --since $W` (#243): how many sessions ran on a stale prompt, and whether the agent or the user acted on it |
| old command names | `pixel recall search "note: '[a-z-]+' is now '" --role tool --since $W`: an agent still calling pre-rename names, i.e. a prompt, skill or script that was never updated; outside the pixel repo only, where the string also sits in `rename_note`'s tests |

Sessions that ran in the pixel repo itself are mostly about pixel's code:
there, "capped" or "error" usually sits in a diff or a test, not in a
pixel answer. Count a hit only when the matched text is pixel's output to
the agent.

`recall` prints timestamps in UTC, the action log's `ts_ms` is epoch: convert
before matching the two.

**C. The pixel repo's own development loop.** Only when the window has
sessions in this repository, or when the question is where agents wait
("bottleneck", "idle", "lead time", « où l'agent attend »). CONTRIBUTING.md
("Agent validation workflow") measures the loop from the first edit to a
fully validated pull request, CI queue and fix/push cycles included; agent
activity alone is not a throughput metric. `lead_time.py` does that per pull
request opened in the window:

```bash
python3 .agents/skills/pixel-retro/lead_time.py $W            # reads gh pr view per PR
python3 .agents/skills/pixel-retro/lead_time.py $W --no-gh    # offline: edit → PR open only
```

It reads the Claude Code transcripts of this repository and its
`.claude/worktrees/` (another path with `--projects <dir>`), and prints per
PR the time from its first edit to `gh pr create`, from there to the last
check completion on the final head, the pushes in that session, and the
session's time split into tool, model, idle (waiting on a background task)
and human; then the median lead time and the commands with the most
blocking time. Read the numbers with their limits:

- **Idle is a ceiling, not a saving.** It is recoverable only where the
  session had an independent unit to advance; the data does not say whether
  it did.
- **One session per PR.** Follow-up pushes from another session count in
  open → green but not in the split.
- **Compare windows, not sessions.** A rule change (#418 on 2026-09-30, say)
  shows as a shift of the median and of the top blocking commands between a
  window before it and one after, quoted with both commands.

A finding here is a workflow change, not a pixel bug: its destination is
CONTRIBUTING.md, `.agents/rules/*.md`, a script under `scripts/` or a CI
workflow, and its report block gives the before/after numbers.

**D. Adherence: do agents use Pixel, and use it well?** Whenever the
question is whether a hook, prompt or routing change moved agent behaviour
("coherence", « est-ce que l'agent utilise pixel », adoption), and in every
retro whose window spans such a change. `adherence.py` reduces each session
in a Pixel-indexed repository, Claude Code and Codex alike, to its retrieval
events and prints per host (never pooled):

```bash
python3 .agents/skills/pixel-retro/adherence.py $W          # table
python3 .agents/skills/pixel-retro/adherence.py $W --json   # for a before/after diff
```

the Pixel share of searches, how often the first search was Pixel, how often
a Pixel call's very next event was a native search, unbounded and wide reads
overall and right after a Pixel call, and the editing sessions that ran
`impact`/`who-calls`/`call-path` before their first edit. The baseline,
`adherence.py 30d` on 2026-10-04, before the #703/#704/#706/#707/#711 fixes:
Claude 503 sessions, Pixel share 0.011 (74 Pixel against 6 458 native
searches), first search Pixel 6 %, a native search right after Pixel 32 %
(24/74), unbounded reads right after Pixel 50 %, impact before the first edit
1 % (3/311); Codex 76 sessions, share 0.059 (85 against 1 350), first search
Pixel 20 %, a native search right after Pixel 22 %, impact before the first
edit 65 % (20/31). Read them
with their limits:

- **It records order, not intent.** A native search right after Pixel is a
  sequence, not proof the answer went unused: it can be the right move (a
  string Pixel does not index). The A/B in `eval/` (#626) says where Pixel
  should win; this says what agents did.
- **Compare windows on the same host**, before and after the change, each
  quoted with its command; a Codex number is never read against a Claude one.

For each action-log error worth keeping, find its transcript turn with a
distinctive token from `args` (a `--request-id`, a path, a pattern):
`pixel recall search '<token>' --since $W`. The turns after it show the
cost: how many calls the agent spent, and whether it fell back to a native
tool.

## Step 2 — Verify each candidate

Session memory and a single log line both lie. For every candidate:

1. **Reproduce on the current binary.** Re-run the same command only when
   it is a read op (`search-*`, `find-*`, `impact`, `who-calls`,
   `call-path`, `scope-task`, `pack-context`, `repo-state`, `review-changes`,
   `diff`, `commit-history`, `action-log` without `--clear`, `recall`
   `search`/`show`/`sessions`/`status`). A repository mutation (`commit`,
   `push`, `new-branch`, `fast-forward`, `sync-branch`, `build-index`) runs
   only inside a throwaway `git init` fixture, never in the user's repo.
   Never replay a host-wide command (`install`, `uninstall`, `self-update`,
   `daemon start`/`stop`, `recall setup`): read its code path instead. No longer reproduces → check `pixel commit-history` /
   `pixel search-history '<token>'` for the fix and mark it `fixed` with the
   commit, not as a suggestion.
   Judge it against the binary that produced the log, not only `main`: a
   fix merged after the installed release (`pixel --version` on that
   machine, `git tag --contains <sha>`) is reported as « corrigé sur `main`
   (#N), pas encore publié », with what the user runs until the release.
   The 2026-09-23 retro of a 0.4.0 machine proposed three changes that
   `main` already carried: the pi guard location (#224), the checkout
   slowdown (#238), `doctor`'s fixes (#240).
2. **Name the cause in the code.** `pixel find-symbol` / `pixel
   search-content` on the error string, then `pixel pack-context` on the
   function that emits it. A suggestion names the file and function to
   change.
3. **Separate pixel from its surroundings.** An `index.lock` held by
   another git process, a pre-commit hook failing, a daemon lock held by a
   live daemon: pixel's part is at most the error wording or a retry, never
   the root cause. Say which.
4. **Measure the cost.** Occurrences, distinct sessions, distinct repos,
   agents, and turns or seconds spent working around it. Quote the command
   that produced each number (`.agents/rules/measuring.md`).

## Step 3 — Classify and rank

| Kind | Destination |
| --- | --- |
| Bug (wrong answer, crash, hang) | `crates/…` fix + a regression test that fails without it (`.agents/rules/mutation-gate.md`) |
| Error that does not say what to do next | the message at its emit site: name the flag, the valid value, the next command |
| Agent misuse of a flag or command | the prompt pixel installs: `crates/pixel-install/assets/pixel-agent-prompt.md` (and `pixel-subagent-prompt.md`), or the clap help text |
| Agent bypassed pixel because the answer was worse | the command's output (truth markers, caps, ranking) — not the prompt; a stronger "MUST" does not fix a weak answer |
| Slow read op | profile first, suggest only with a measured number |
| Install, doctor or daemon drift | first check whether a rerun of the install would clear it, without running it on the user's machine (Step 2 forbids replaying host-wide commands, and `--repo` writes into their repository): run it against a copy, `HOME=<scratch> CODEX_HOME=<scratch>/.codex XDG_CONFIG_HOME=<scratch>/.config pixel install --shell <shell>` then `pixel doctor` with the same three variables (all on the command line: `CODEX_HOME` and `XDG_CONFIG_HOME` otherwise point the install back at the real files), or for a `repo.*` check `pixel install --repo <fixture>` on a `git init` fixture seeded with the drifted file; failing that, read the install code path. If a rerun clears it, the install is fine and the friction is that nobody reran it: an upgrade path that skips it, a note nobody read. Propose a change to `pixel-install` only when the install itself leaves the check red, or a `pixel doctor` check when nothing reported the drift at all |
| Repo rule or skill gap | `.agents/rules/*.md` or `.agents/skills/*/SKILL.md` |
| Not pixel's (user repo, git, another tool) | one line in the report, then drop it |

Score = occurrences × cost per occurrence (turns or seconds) × spread
(repos × agents). Rank on it; break ties toward the smaller change. Keep
the top five; the rest go under "also seen" in one line each.

## Step 4 — Report, then stop

Terminal output, in French, one block per suggestion:

```
### N. <titre court> — <kind>, score S
Symptôme : ce que l'agent a vu (la commande, le message d'erreur exact)
Preuves : <n> occurrences, <s> sessions, <r> repos · agent:id #turn, invocation_id
Reproduit sur <version> : oui / non (corrigé par <sha>) / corrigé sur main (#N), pas encore publié
Route (lente) : <route> <reason> · <phase dominante> <ms> — ligne `slow.py`
Cause : <fichier>:<fonction> — une phrase
Proposition : le changement, où, et le test qui le prouverait
Effort : S / M / L
```

End with the dropped items (not pixel's, already fixed, already in the
ledger) and ask which suggestions to implement. Implementation then follows
CONTRIBUTING.md and the PR doctrine, one branch per suggestion.

## Guardrails

- **Transcripts from other repos carry business data.** Quote only pixel
  commands, pixel output and error text. Never copy a customer, subscriber,
  plate, email, amount or commit message body from any repository other than
  pixel, neither into the local report nor into anything public: an issue or
  PR on the pixel repo gets a generic reproduction (`git init` fixture,
  placeholder paths).
- **Nothing leaves the machine without the user's go.** No `gh issue
  create`, no PR, no comment until the user picks a suggestion in the chat.
- **Read-only by default.** Reproductions of mutation ops run in a temp
  fixture; `pixel action-log --clear` is never part of a retro.
- **A miss is not an absence.** "No friction found" means none in the
  indexed agents over the window; say which agents were indexed and fresh.

## Ledger

After the user answers, append one row per reported or dropped item. The
values are data, never source: a fingerprint is built from error text,
which can hold `$(…)`, quotes or newlines. Write the items as a JSON list
with the file-writing tool (not through a shell command), in the scratchpad,
then hand the file to `ledger.py`, which validates every item (verdict one of
`suggested`, `picked`, `fixed`, `wontfix`, `not-pixel`; ref an `agent:id
#turn` or a GitHub URL), refuses the whole file on one bad item, and appends
with tabs and newlines flattened:

```json
[{"fingerprint": "<command>|<normalised error>", "verdict": "suggested", "ref": "claude:5e6585e2 #300"}]
```

```bash
python3 .agents/skills/pixel-retro/ledger.py <scratchpad>/pixel-retro-items.json
```
