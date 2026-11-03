---
name: agent-session-debugging
description: Operate real pi, agy, Claude, and Codex sessions in Herdr to diagnose tool behavior from transcripts, ask a CLI directly when its own behavior is unclear, then implement, retest, and redeploy a verified fix.
---

# Debugging real agent sessions

Use this skill when an agent ignores Pixel guidance, a tool result is unclear,
metrics appear missing, or an installed hook/prompt may behave differently from
tests. The operator coordinates the investigation from the existing main pane;
the four agent panes are independent observers and reproducers.

## Establish the Herdr workspace

Use Herdr's native agent commands for pane-to-pane communication. Do not use
ACP, generic subagents, raw pane input, or another messaging system to talk to
these sessions. Set `HERDR_ENV=1` on every Herdr invocation.

Reuse the active workspace and keep its main orchestrator pane on the left.
Arrange pi, agy, Claude, and Codex as a 2×2 grid on the right. Inspect
workspace, tab, pane, and layout state before changing it. Rebuild the active
tab's layout and close its existing non-orchestrator panes when needed to get
the requested view; preserve the orchestrator pane. Do not create a fresh
workspace/tab when the current one can be arranged. Keep layout operations
unfocused unless the operator explicitly asks to change focus.

For a clean 2×2 on the right, close only the active tab's non-orchestrator
panes if they prevent the layout, then use this split sequence. After each
split, take the new `pane_id` from Herdr's result and substitute it into the
next command:

```sh
rtk env HERDR_ENV=1 herdr pane split <main-pane-id> --direction right --ratio 0.5 --cwd <repo> --no-focus
rtk env HERDR_ENV=1 herdr pane split <right-region-pane-id> --direction down --ratio 0.5 --cwd <repo> --no-focus
rtk env HERDR_ENV=1 herdr pane split <top-row-pane-id> --direction right --ratio 0.5 --cwd <repo> --no-focus
rtk env HERDR_ENV=1 herdr pane split <bottom-row-pane-id> --direction right --ratio 0.5 --cwd <repo> --no-focus
```

The resulting cells are top-left, top-right, bottom-left, bottom-right. Start
one agent in each existing cell, unless that cell already hosts the intended
agent:

```sh
rtk env HERDR_ENV=1 herdr agent start pixel-pi --kind pi --pane <top-left-pane-id>
rtk env HERDR_ENV=1 herdr agent start pixel-claude --kind claude --pane <top-right-pane-id>
rtk env HERDR_ENV=1 herdr agent start pixel-agy --kind agy --pane <bottom-left-pane-id>
rtk env HERDR_ENV=1 herdr agent start pixel-codex --kind codex --pane <bottom-right-pane-id>
```

Verify with `herdr pane layout` and `herdr pane list`; the main pane must remain
left, the four cells right, and all four intended agent kinds present. Do not
close the main pane or create another tab merely to make the layout fit.

Run split and `agent start` commands sequentially, taking each new `pane_id`
from the previous result. Parallel Herdr CLI calls race: two simultaneous
`pane split` calls made both new panes vanish before `agent start` could use
them, and a fresh pane is not immediately an "available shell" — if
`agent start` fails with `agent_pane_busy`, wait a few seconds, close that
pane, and split again.

A newly started Codex in this repository opens Pixel's hook-review dialog
("⚠ N hooks need review before they can run", footer `t trust all · esc close`)
and Herdr classifies it `idle`, so the first `agent prompt` fails with
`agent_prompt_stalled` while the dialog is up. Review the pending definitions
in `/hooks` before the first prompt. Trust only the hooks whose commands you
have inspected; the review is keyed by the `hooks.json` path, so every new
worktree starts untrusted (the `repo.codex-hook-review` doctor check):

```sh
rtk env HERDR_ENV=1 herdr agent send-keys <codex-name> esc # close the dialog after reviewed hooks are trusted
```

`esc` only closes the dialog after the reviewed hooks are trusted; it does not
approve hooks. Only prompt after `herdr agent get` shows a changed
`state_change_seq` and the dialog text is gone from `herdr agent read`.

Start or address agents with Herdr's `agent` commands, for example:

```sh
rtk env HERDR_ENV=1 herdr agent prompt <agent-name> "<one neutral diagnostic prompt>" --wait --timeout 120000
rtk env HERDR_ENV=1 herdr agent read <agent-name> --source recent-unwrapped --lines 120
rtk env HERDR_ENV=1 herdr agent get <agent-name>
```

Check the local Herdr help for exact pane/tab/layout syntax before operating
the workspace. Use returned pane and agent IDs rather than guessing IDs.

## Diagnose one behavior at a time

1. State one observable question: for example, did the agent use Pixel before
   reading code, scope reads after a Pixel hit, or receive Pixel's metrics line?
2. Give each agent one neutral, read-only prompt. Ask for exact commands,
   results, files/ranges read, and whether it saw metrics; ask it to distinguish
   direct observations from inference. Do not lead it toward the expected answer
   or repeatedly prompt until it agrees.
3. Read each real `recent-unwrapped` transcript after the turn settles. Inspect
   prompt, tool calls, results, and final answer in order. A compact UI summary
   or settled agent state is not a transcript.
4. If the CLI's own UI hides relevant detail, ask that CLI directly in its
   diagnostic prompt (for example, request the exact read ranges behind a
   displayed `Read(file)`). Record the answer as CLI self-report; it does not
   independently prove hidden tool arguments.
5. Compare transcript evidence with the expected contract. Pixel `path:line`
   hits should be followed by bounded reads, such as
   `sed -n '<line>,+40p' <path>` or an offset/limit read. Verify the actual
   command and output, not just the agent's claim that it used Pixel.
6. Separate model behavior from integration behavior. If metrics are missing,
   check stderr redirection/suppression and the hook relay; do not conclude
   Pixel failed to emit just because the model did not mention the line.
7. Record evidence as directly visible, CLI self-report, inference, or unknown.
   Preserve exact commands/results and state what remains unverified.

## Pixel retrieval and metrics evidence

- Record the exact Pixel command, complete result, and any `path:line` served.
- Treat `0 matches` as a normal retrieval result with a recovery step, not as
  task completion: retry once with `pixel find-code` using the task's behavior
  or concept. If that does not converge, follow the task route's bounded native
  fallback. Do not repeatedly vary Pixel queries without a new information need.
- A high prior-call note (for example, “42 prior calls in 10 minutes”) is
  informational by itself. Check whether the current query has a relevant hit;
  call volume alone is not evidence of a loop and must not block retrieval.
- Record each subsequent bounded read and its visible range. Do not infer ranges
  from an abbreviated pane label.
- Copy the metrics line verbatim from that invocation's result, correlated to
  that invocation. Never reconstruct it or use a global “latest” line.
- Keep stderr visible while testing metrics. Do not redirect or suppress it
  (including `2>/dev/null` or `2>file`).
- Track emission, host relay, display, and model use as separate events; name
  where the line was observed. A relayed line does not prove the model noticed
  or acted on it.

For a cross-harness retrieval challenge, use one identical read-only task that
has no exact identifier, then inspect each transcript for the first
`find-code` action, one bounded read from a returned location, the exact
invocation's metrics line, and any hook denial. Record unavailable Pixel and
empty-result recovery as successful fail-open paths when the agent continues
with the prescribed fallback. Never count an informational prior-call warning
as a failure on its own.

## Implement, retest, and redeploy

Keep the first agent experiments read-only: do not change project files, hooks,
or settings as part of behavior measurement. Once evidence isolates a cause,
the operator implements the smallest fix in the project. Preserve the transcript
and repro, add or update a regression test for the observed contract, then run
the focused tests and repository-required gates. For installed behavior, verify
the installed binary and a real hook/session payload before investing in unit
test churn; unit-only success does not establish installed behavior.

After the fix, rerun the same neutral prompt/condition in real sessions, inspect
the new transcript, and compare the same observable. Redeploy only through the
repository's documented install workflow, then verify the installed result.
Never claim success from a prompt change, passing unit test, or install command
alone. Report what changed, the test and deployment evidence, direct observations
versus self-reports, and any remaining uncertainty.

If an agent unexpectedly edits a file during a read-only experiment, stop the
experiment and report the path and observed change; do not discard or overwrite
it. Keep sessions, settings, and unrelated workspace panes intact.

## Findings record

For each agent run, retain a concise record:

| Field | Record |
| --- | --- |
| Agent/session | Agent kind, name, and inspected session |
| Prompt | Exact single prompt sent |
| Transcript | `recent-unwrapped` source and relevant excerpt |
| Pixel retrieval | Exact command and complete result |
| Reads | Exact bounded read commands/ranges, or `not exposed` |
| Metrics | Exact line and where it appeared, or `not observed` |
| Evidence class | Directly visible / CLI self-report / inference / unknown |
| Conclusion | Narrow supported finding and remaining uncertainty |
| Fix/verification | Changed behavior, regression test, real-session retest, deployment evidence |

Keep separate runs separate. Example observations from the Pixel enforcement
investigation: Claude's controlled transcript showed a `PostToolUse:Bash`
metrics relay, while Claude self-reported that stderr redirection hid metrics in
another run; Devin's transcript showed Pixel search followed by scoped reads;
agy's compact UI hid individual ranges, while agy self-reported five bounded
reads totaling 242 lines. Do not upgrade any self-report into directly observed
evidence.
