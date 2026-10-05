# Manual side-by-side: Codex ± Pixel in Herdr panes

A cheap, interactive complement to two scripted harnesses that already exist:

- `eval/arena.sh` — dockerized arms (`raw` vs `pixel` vs semble/graft/
  stacklit/gitnexus/gortex), identical snapshot/auth/model/prompts;
  `--watch` opens one Herdr pane per arm container. Past run artifacts live
  in `target/codex-pixel-ab/run-*/` (gitignored) and Oct-3 tmux scrollback is
  captured in `2026-10-03/`.
- `eval/run.sh` — the injection-balance loop (`baseline`/`quiet`/`full`
  arms, scored by `score.py` + `gate.py`).

This directory covers the *manual* variant: two Codex panes share the same
snapshot worktree and receive the same tasks; one is instructed to prefer
`pixel` commands, the other is forbidden from using them. Compared by reading
the pane transcripts.

## Repeatable process

1. **Snapshot the repo** to a scratch dir so both arms see identical files
   and neither pollutes a real checkout:

   ```bash
   git worktree add /private/tmp/pixel-ab-$(date +%y%m%d) <commit>
   # or: rsync -a --exclude .git --exclude target ~/Documents/pixel/ /private/tmp/pixel-ab-$(date +%y%m%d)/
   ```

   Build the index once for the pixel arm: `pixel build-index` inside the
   snapshot (a shared `.pixel/` index is fine — only the *agent's retrieval
   route* differs between arms, not the data).

2. **Open two sibling panes** next to yours (from inside Herdr):

   ```bash
   herdr pane split --current --direction right --cwd /private/tmp/pixel-ab-<date> --no-focus
   # read .result.pane.pane_id, then:
   herdr agent start ab-plain --kind codex --pane <pane-id>
   herdr pane split --current --direction right --cwd /private/tmp/pixel-ab-<date> --no-focus
   herdr agent start ab-pixel --kind codex --pane <pane-id>
   ```

   Operator layout: **three panes on one row** (operator | ab-plain |
   ab-pixel) — split `right`, never `down`. If a pane lands in the wrong
   place, `pane move` within the same tab is a `same_tab` no-op; route it
   through a temp tab: `pane move <id> --new-tab --no-focus` then
   `pane move <id> --tab <tab> --split right --target-pane <col> --no-focus`.
   Then label everything: `herdr tab rename <tab> "codex ±pixel"`,
   `herdr pane rename <pane> ab-plain|ab-pixel`.

3. **Send the same task to both**, differing only in the retrieval
   instruction:

   - `ab-plain`: "… Do not use the pixel CLI; use only grep/read."
   - `ab-pixel`: "… Prefer pixel commands (find-code, who-calls, impact)
     before any grep or file read."

   ```bash
   herdr agent prompt ab-plain "<task>" --wait --timeout 300000
   herdr agent prompt ab-pixel "<task>" --wait --timeout 300000
   ```

4. **Capture before closing.** Read each pane transcript first — closing a
   pane destroys its scrollback:

   ```bash
   herdr agent read <pane-id> --source recent-unwrapped --lines 300 > ab-plain.txt
   herdr agent read <pane-id> --source recent-unwrapped --lines 300 > ab-pixel.txt
   ```

   Also copy any files the arms produced in the snapshot
   (`results_*.md`, `tools/…`).

5. **Close the panes** — always, even panes you did not create, once the
   transcripts are captured:

   ```bash
   herdr pane close <pane-id>
   ```

6. **Compare**: wall time ("Worked for Xm Ys"), number of retrieval calls,
   read widths after a `path:line` hit, correctness of the final answer, and
   whether the pixel arm's `🟩 pixel …` metrics lines appeared.

## Run 2026-10-05

Directory: `2026-10-05/` — `ab-plain.txt`, `ab-pixel.txt` (pane transcripts),
`results_plain.md`, `results_pixel.md`, `count_signals.py` (produced
artifacts). Snapshot: `/private/tmp/pixel-ab-251005`, pixel-rank +
pixel-session crates only. Codex `gpt-5.6-terra medium`, panes `w4:p2T`
(plain) and `w4:p2S` (pixel), both closed after capture.

| | ab-plain | ab-pixel |
|---|---|---|
| Task 1 (trace ranking) | 2m 21s; rg+rtk reads only; correct chain, all 23 test callers listed | 3m 3s; find-code→impact→who-calls→search-content then bounded sed reads; same chain + noted graph is non-closed-world and bm25_rank is unused |
| Task 2 (count_signals.py) | 1m 2s; wrote script, diff-verified | 1m 23s; find-code located the script the plain arm had just created in the shared snapshot, reused it; wrote a Markdown table instead of raw lines |
| Retrieval calls | 6 grep/read operations | 6 pixel calls (2 find-code, 2 impact, 1 who-calls, 1 search-content) + bounded sed reads ≤40–70 lines |

Confound to note when repeating: **the arms share one worktree**, so task 2's
pixel arm could see `tools/count_signals.py` already written by the plain
arm's task 1 file + the index picking it up. For file-producing tasks, give
each arm its own snapshot or run the arms' edits under different filenames
(the prompts already did: `results_plain.md` vs `results_pixel.md`).
