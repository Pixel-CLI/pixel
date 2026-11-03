---
name: probe
description: "Create or refine a brief probe, make a question zero-call, or run /probe \"<question>\" [--repo path]: run the question with the current Pixel brief, stop if it is already zero-call, otherwise translate the calls the agent still made into evidence, write a generic probes/<name>/ script, prove it offline and live, and hand back a report and a commit."
---

# Make a question zero-call with a probe

A probe is a script in `probes/<name>/` that adds evidence to the prompt-submit brief (contract: CONTRIBUTING.md,
"Write a probe"). `/probe "<question>" [--repo <path>]` does the whole job from a plain prompt. Prototype in a
script first (`.agents/rules/prototype-first.md`); the Rust is written only for a KEEP, as a step (step 8), never to
find out whether an idea works. Use the frozen `pixel` (`--bin`), never a rebuild while validating.

**Agent.** The scripts take `--agent codex|claude` (default: the agent this runs under, `claude` inside Claude Code;
`PIXEL_PROBE_AGENT` overrides). Claude runs use `claude -p --output-format stream-json` with a per-run settings
file (the prompt-submit hooks, or none for the vanilla arm), your OAuth login and no `ANTHROPIC_API_KEY`; they draw on
the Claude quota, so pass `--agent codex` for heavy loops. Both streams are normalised into the same call shape
(Codex commands and Claude Read/Grep/Glob/Bash) before translation.

## The flow (what `scripts/bench/probe-loop.py` does, and what you decide)

1. **Register** the question: the prompt on `--repo` (default: the current repository) pinned at HEAD, stored in
   the git-ignored `eval/brief-bench/adhoc/`.
2. **Run the current Pixel brief live, 1 run.** This is both the zero check and the baseline call count; there
   is no separate baseline run. If it makes 0 calls, STOP: report "already zero-call" with the answer and the
   brief size. Nothing else runs.
3. **Only above 0:** run the agent without Pixel (no hook, 1 run) for its calls and final answer. Each question
   costs at most two live runs before any probe work. The `path:line` the
   answer cites become the question's oracle lines.
4. **Translate.** First the brief-arm calls (exactly what the agent still needed on top of the brief), then the
   vanilla calls (coverage): each tool call becomes the deterministic evidence that would have removed it (a
   `sed -n a,bp` is that region, an `rg` the matching lines by file, a README read a documentation section,
   `git log/show` a history block, `--help` the CLI definition region), with: already in the brief (yes / buried at
   line N / partly / no) and which probe of `probes/` covers it, or "no probe: candidate". Classify each open row:
   **missing** (not in the brief), **buried** (in it, far from the top or among noise; pixel-4's answer sat at line
   145), **wrong** (it points at the wrong code; excalidraw-5's truncated body and noisy trace).
5. **Fix (you decide).** If an existing probe almost covers it, refine that probe (gating, ranking, context). Else
   `cp -R probes/_template probes/<name>` and write a **generic rule**. State a one-line hypothesis first, e.g.
   "ripgrep-5 loses its calls if the brief carries the answering commit's patch".
   **Overfitting guard.** The probe must derive every word, path and symbol from the prompt and the repository at
   run time. Code that spells out words, paths or symbols of this one prompt is refused: `probe-test.py --guard
   "<prompt>" --guard-paths <oracle paths>` (and the loop runs it) fails it with the literals it found.
6. **Validate.** `python3 -I scripts/bench/probe-loop.py --prompt "<question>" --repo <path> --probe <name>
   --hypothesis "<one line>"`: the guard, offline `probe-test` on the question and on every bench question (no
   regression), then live with and without the probe (`--live-reps 1` by default). KEEP when the calls drop,
   ideally to 0. If not, translate the new remaining calls and loop: at most ~3 iterations (`--max-iter`), each
   logged as hypothesis -> predicted -> live -> kept/dropped.
7. **Hand back** a report: calls before -> after, the probe diff, the bench delta. With the user's ok, commit the
   probe on the current branch; a KEEP becomes a PR (`Refs #906`), a DROP is reported and not merged.
8. **Ship a KEEP as a step.** The script stays in `probes/<name>/` (unregistered) as the oracle; the shipped form is
   `crates/pixel/src/execution_brief/steps/<name>.rs`: a `pub(super) const STEP: Step` (the manifest's `when_*`,
   `budget_ms`, `mode`) whose `run(&Ctx) -> String` returns what the script printed, plus one name in `register!`
   (`steps.rs`). Port the script's cases as the step's tests (`steps::testing::repo` builds the git fixture), then run
   `python3 -I scripts/bench/step-parity.py <name> --bin <pixel built before the port>`: both arms' `probe-test` verdicts
   and `identical` on every question. A step has no process to kill: it must not panic (release aborts) and checks
   `ctx.timed_out()` in its loops.

For a question already in a recorded set use the same harness directly: `brief-check.py --all`,
`probe-translate.py <id>`, `probe-test.py <name> [--live]`. The default set is the **public bench**
(`eval/brief-bench/questions.jsonl`, ids like `express-07`, with oracle lines and plain-Codex baselines): it works on a
clean checkout, the scripts clone the 7 pinned repositories on demand under `$PIXEL_CENSUS_ROOT` (default
`$TMPDIR/pixel-tool-census`), and a question with no recorded run replays its bench baseline. `--questions <file>`
names another file; `--census-db <census.db>` is the maintainer's private census (ids like `pixel-4`, local only:
never commit or quote its prompts). Examples in this skill that name a census id need that option.

## Traps (each cost a run)

- **Probes interact, so a combination needs a full-set run.** The first chain of every prototype on every question
  fixed excalidraw-3, ripgrep-3 and ripgrep-5 and regressed requests-3 (0 -> 4), requests-4 (0 -> 10), requests-5
  (0 -> 4) and excalidraw-4 (7 -> 13): the reordering and the extra blocks made Codex explore more. Gate each probe
  (`skip_kinds`, `when_*`), and before any combined claim run the whole set
  (`arena.sh run-set census --brief-extra "python3 scripts/bench/probe-chain.py"`) and keep every zero-call question at 0.
- **A probe must see the whole brief.** The first wrapper fed probes only part 1's slice, and a replacing probe
  then dropped parts 2 and 3. The shims now join all parts before probing and split the result again.

- **A single run can be lucky** (the same empty brief took 8 calls once and 11 the next time). Re-run with
  `--live-reps 3` before claiming a 0 on a borderline result, and say min-max.
- **Parallel live runs collided on the shared cache's `index.lock`.** `brief-zero-calls.py` now locks around the
  source's `prepare` (the pinned clones under `<root>/cache`); the harness keeps one clone per session. Do not run your own `git` in a shared cache.
- **A confidently wrong brief is worse than none.** The strict config registry answered 3000 when the dev port was
  3001 because it had not read the tracked `.env.development`. Say what you could not see.
- **Gate every probe on intent.** The ungated config registry injected noise into nine prompts. Use `when_kinds`,
  `skip_kinds` (the brief's `kind` is null for a weak, low-tier brief) and `when_prompt`.
- **Generic words match everything.** "file", "for", "keep", "element", "scene" made false matches: keep a
  generic-word list and weigh those words down; rank by how few sections or names carry a word (IDF).
- **A hook carries about 9.5 KB.** The brief is split over 3 parts; a bigger probe output is cut by the merge, the
  append blocks first. Keep blocks small and put the answer first.
- **Only git-tracked `.env*`, with secret-looking values masked.** The probe is read-only and uses no network.
- **A nested clone corrupted the pixel results.** `cp -R` into an existing directory nests the repository one
  level down; `brief-check.py` now locks and verifies the clone (f50510f2). If paths in a brief start with the
  repository name, suspect the clone first.
- **Replay cannot credit broad `rg`/`sed` ranges**, nor predict a gain from ranges the old brief never named
  (excalidraw-5 replayed 9 -> 9 and went to 0 live). Live is the arbiter; the replay only guards.
- **A buried answer is a ranking problem**, not a missing one: reorder (`first:`), drop the noise, give the
  constant 12 lines of context and its uses.
- **Determinism.** `rg` and `git` outputs must be sorted (`--sort path -j 1`); the harness runs the probe twice.
