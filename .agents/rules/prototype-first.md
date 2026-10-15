# Prototype First, Then Integrate

Always loaded: a change whose value is uncertain (a retrieval or brief
heuristic, a new evidence source, a ranking tweak, a gate threshold) is
proven with a throwaway script before any Rust is written. A Rust edit costs
a compile, a test run and a review; a wrong hypothesis paid for in Rust is
paid several times.

- **State the hypothesis in one line.** It names the question or case, the
  added input and the expected effect: "ripgrep-5 reaches 0 tool calls if the
  brief carries the `git log -S<term>` hits with sha, date and subject".
- **Prototype outside the crates.** A Python or shell script of about a
  hundred lines, in the session scratchpad and not committed, produces the
  extra evidence with plain `git`, `rg` or a tree-sitter CLI, and appends it
  to the current output (`pixel brief`, `pixel search-meaning`, …) for that
  one case. No edit under `crates/` at this stage.
- **Validate with the fastest check that can falsify it.** For the brief that
  is a deterministic, LLM-free check first (oracle-line coverage, or a replay
  of the tool calls a previous live run made, counting the ones the new
  evidence now covers), and a live agent run of that single case only to
  confirm a predicted win. Never a full multi-session campaign per idea.
- **Drop a hypothesis that does not move the number.** Record it as tried →
  predicted → measured → dropped, and try the next one, including
  alternatives found by searching how other tools and papers solve the same
  problem, not only extensions of what pixel already does.
- **Port only what was validated, one change at a time.** The Rust change
  reproduces the prototype's behaviour, carries its tests, and is
  re-measured with the same command as the prototype (`measuring.md`).
- **Not for bug fixes with a known cause.** A defect whose fix is already
  clear goes straight to a test and the fix; this rule is for changes whose
  benefit is a bet.
- **A prototype that works becomes a probe.** A brief heuristic proven by a script is shipped as
  `probes/<name>/` (a generic script plus its manifest), judged by `scripts/bench/probe-test.py` and written with the
  `probe` skill (`.agents/skills/probe/SKILL.md`); not as a Rust change to the brief.
