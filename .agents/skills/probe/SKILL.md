---
name: probe
description: "Create or refine a brief probe, make a question zero-call, or run /probe: diagnose what a session still looks up after the prompt-submit brief, write a probes/<name>/ script, prove it with scripts/probe-test.py offline and live (median of several Codex runs), and open a PR for a KEEP."
---

# Make a question zero-call with a probe

A probe is a script in `probes/<name>/` that adds evidence to the prompt-submit brief (see CONTRIBUTING.md,
"Write a probe", for the contract). This is the loop that produced the first five. Prototype in a script first
(`.agents/rules/prototype-first.md`); Rust is for the brief's integration, never for one probe's idea.

## Procedure

1. **Pick a question still above 0 calls.** `python3 -I scripts/brief-check.py --all` (the last live run's calls
   beside the replay), or read `.pixel/brief-feedback.jsonl` of a real session. The frozen binary, never a rebuild:
   `--bin <pixel>`; `--build` is opt-in.
2. **Diagnose.** Dump the brief (`brief-check.py <id> --show-brief`), list what each recorded call looked for
   (`brief-check.py <id>` prints the uncovered targets), and classify the answer:
   - **missing**: not in the brief at all (a file the words never name, a git diff, a registry);
   - **buried**: in the brief, but far from the top or among noise (pixel-4's answer sat at line 145);
   - **wrong**: the brief points at the wrong code (excalidraw-5's trace and its truncated body).
   Only missing needs a lookup; buried needs `drop:`/`first:`; wrong needs `drop:` plus a better block.
3. **One-line hypothesis**, e.g. "ripgrep-5 loses its calls if the brief carries the answering commit's patch".
4. **Copy the template**: `cp -R probes/name-files probes/<new-name>`, edit `pixel-plugin.toml` (`name` equals the
   directory, `when_kinds`/`skip_kinds`/`when_prompt`, `budget_ms` at most 1000, `mode`) and the `probe` script.
5. **Offline**: `python3 -I scripts/probe-test.py <new-name> --bin <frozen pixel>`. Expect `OK`: contract,
   budget, deterministic twice, read-only, no stray lines, no touched question worse. Add `--no-history` for a
   history probe's real state, `--ids a,b` to focus. The replay credits only ranges the brief now quotes, so an
   offline `OK` with no gain is normal; it is the live run that decides.
6. **Live** (the verdict): `probe-test.py <name> --live --bin <frozen pixel>` runs Codex on the target questions
   with and without the probe, `--live-reps` (default 3) times per arm, and reads the median with min-max. KEEP
   needs the median of a target to drop and none to rise. Re-confirm any single-run 0.
7. **Log and ship.** Write the line `PROBE <name> | questions | predicted a -> b | coverage x -> y | live m -> n |
   KEEP` in the PR, with the `report.md`. A KEEP becomes a PR (`Refs #906`); a DROP is reported with the same
   numbers and not merged.

## Traps (each cost a run today)

- **Codex varies a lot.** The same empty brief took 8 calls once and 11 the next time. One run proves nothing: use
  the medians of at least 3 per arm, and say min-max.
- **Parallel live runs collided on the census cache's `index.lock`.** `brief-zero-calls.py` now locks around
  `prepare_cache`; keep one clone per session (the harness does) and do not run your own `git` in the cache.
- **A confidently wrong brief is worse than none.** The strict config registry answered 3000 when the dev port was
  3001 because it had not read the tracked `.env.development`. Say what you could not see.
- **Gate every probe on intent.** The ungated config registry injected noise into nine prompts. Use `when_kinds`,
  `skip_kinds` (the brief's `kind` is null for a weak, low-tier brief) and `when_prompt`.
- **Generic words match everything.** "file", "for", "keep", "element", "scene" made false matches; keep a
  generic-word list and weigh those words down.
- **A hook carries about 9.5 KB.** The brief is split over 3 parts; a bigger probe output is cut by the merge, the
  append blocks first. Keep blocks small and put the answer first.
- **Only git-tracked `.env*`, with secret-looking values masked.** Never read an untracked one; the probe is
  read-only and uses no network.
- **A nested clone corrupted the pixel results.** `cp -R` into an existing directory nests the repository one
  level down; `brief-check.py` now locks and verifies the clone (f50510f2). If paths in a brief start with the
  repository name, suspect the clone first.
- **Replay cannot credit broad `rg`/`sed` ranges**, nor predict a gain from ranges the old brief never named
  (excalidraw-5 replayed 9 -> 9 and went to 0 live). Live is the arbiter, the replay only the guard.
- **A buried answer is a ranking problem**, not a missing one: reorder (`first:`), drop the noise, give the
  constant 12 lines of context and its uses.
- **Determinism**: `rg` and `git` outputs must be sorted (`--sort path -j 1`); the harness runs the probe twice.
