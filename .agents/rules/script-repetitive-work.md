---
paths:
  - "**/*"
---

# Script Repetitive, LLM-Free Work

When a task asks for something repetitive that an LLM judgment does not
change — running a sweep of many prompts against the CLI or the brief,
checking every case of a list, measuring N runs of the same command, replaying
fixed inputs — do not run the commands one by one in the session. Write one
script that enumerates the cases deterministically and runs them all in a
single invocation.

- **Enumerate, then loop.** The case list lives in the script (a literal array
  or generated from a fixture), not in a chain of tool calls. One tool call
  produces all the results; the transcript stays small and the run is
  replayable.
- **Print a verdict per case and a summary.** Each case reports pass/fail with
  the evidence (the route, the count, the exit status), and the script ends
  with a total. A run whose output needs a human to eyeball N lines is not
  finished; the summary line is the result.
- **Exit non-zero on any failure** so a background or CI run fails loudly.
- **Keep it runnable again.** A sweep worth running once (the prompt sweep
  under `/tmp`, a bench harness, a fixture replay) is worth being able to
  re-run after the next edit; prefer committing the script under `scripts/`
  when it checks a lasting property, and leave throwaway validation scripts in
  the session scratchpad when it was a one-off.
- **Same rule at every size.** Two identical commands with a differing
  argument already qualify: write the loop, not the pair.

This complements `prototype-first.md` (which scripts a *hypothesis*) and
`measuring.md` (which says what a number must carry): here the point is that
the *execution* of repetitive checks is batched into one deterministic script
instead of N interactive tool calls.
