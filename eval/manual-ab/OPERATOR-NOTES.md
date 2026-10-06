# Operator notes — manual Codex ± Pixel A/B

Directives from the operator (Livio), recorded verbatim-ish on 2026-10-05.
Treat as standing instructions for this process.

1. **Close panes first, always.** "If there are two panes that are open, they
   should be closed first, even if they are not mine." — Before analysing or
   reporting, close the A/B Herdr panes. Applies to any open agent panes
   involved in the comparison, regardless of who created them. (Capture
   transcripts first — closing destroys scrollback; this session: captured to
   `2026-10-05/ab-plain.txt`, `ab-pixel.txt`, then closed `w4:p2T`,
   `w4:p2S`.)

2. **Save the process into the project.** "The process we're going to repeat
   again. You need to save it into the project." — Done:
   `eval/manual-ab/README.md` holds the repeatable recipe; each run's
   evidence goes in `eval/manual-ab/<YYYY-MM-DD>/`.

3. **Keep evidence.** Transcripts, produced files, pane IDs, model, times —
   store under the dated run directory, not just in chat.

4. **Task intent:** "codex without pixels" = compare Codex with vs without
   Pixel (`ab-plain` / `ab-pixel` arms). "There is an eval" referred to this
   pane-based comparison living next to `eval/run.sh`, the automated loop.

5. **Take notes of everything said** — this file is the running record. Add
   new directives below as they arrive.

6. **Layout: three panes on one row.** Operator prefers the A/B panes as
   columns beside the operator pane — not stacked. `pane move` refuses
   same-tab repositioning (`same_tab` no-op); workaround: `pane move <id>
   --new-tab --label tmp --no-focus`, then `pane move <id> --tab <tab>
   --split right --target-pane <col> --no-focus` (the temp tab auto-closes).

8. **Restart = close and reopen the pane.** Never quit/restart the agent
   inside its pane (no ctrl+c, no `/quit`). `herdr pane close` the arm panes,
   `pane split` fresh ones, `agent start` again. Repeat of directive #1's
   spirit: panes are disposable.

9. **Pin the model at spawn.** `agent start <name> --kind codex --pane <id>
   -- -m gpt-5.6-terra -c 'model_reasoning_effort="medium"'` — verify the
   footer reads `GPT-5.6-Terra medium fast` before sending tasks. Bare
   `codex` picks up whatever default the profile has (caught: `Sol high
   fast`).

7. **Always label the surfaces.** Rename the tab (`herdr tab rename <tab>
   "codex ±pixel"`) and each pane (`herdr pane rename <pane> ab-plain` /
   `ab-pixel`) when the pair is spawned — the operator navigates by labels.

## Open items

- Automated `eval/run.sh` campaign (`baseline` vs `quiet`, REPS=3) launched
  in background this session before the manual setup was understood — running
  as shell `97911b`, results → `eval/results/ab626-codex`. Keep or kill:
  awaiting operator decision.
