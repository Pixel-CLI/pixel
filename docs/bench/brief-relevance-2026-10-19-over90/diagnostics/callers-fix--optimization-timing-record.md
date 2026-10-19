# Caller-probe timing record

This is a compact record of the direct semantic-fallback timing observations.
The final optimized command printed a byte count only; its generated text was
not persisted, so the byte count does not establish output equivalence.

## Before candidate-name prefilter

- Source revision: cached export-walk/mask-reuse caller probe, SHA256
  `0f6978db80e874796684449251f6da21671ddf5963e745f03447bbb031fa2e98`.
- Prompt: `Show callers of the function that creates fresh drawing element IDs.`
- Clone: `/tmp/pixel-brief-over-90/candidate-root/cache/excalidraw__excalidraw`
- Observation recorded from the controlled profile: scan 136.8 ms, semantic
  fallback 6,362 ms, total 6,498.8 ms; 718 files, complete; output 1,943 B.
- Profiled diagnostic from the same pinned source snapshot before cache reuse:
  9,767 import-resolution checks across 492 `imported_local_names` calls;
  `module_exports_name` had 73,853 calls and 24.482 s cumulative under
  cProfile. Profile overhead makes those values diagnostic, not wall time.

## Candidate-name prefilter timing

- Source state: first candidate-name filter with cache reuse; no source hash was
  recorded at invocation. It predates the final once-per-snapshot edge-map
  extraction and the local-export alias fixture.
- Command shape: import `probes/cand-j-callers/probe`, call `tracked_source`
  then `semantic_caller_blocks` with the prompt and clone above, while holding
  `/tmp/pixel-benchmark.lock`.
- Observation: scan 86.8 ms, semantic fallback 1,331.5 ms, total 1,418.3 ms;
  718 files, `partial=False`, output count 3,869 B.
- The 3,869-B text was not saved. It cannot be diffed against the earlier
  1,943-B saved direct output at
  `/tmp/pixel-brief-over-90/callers-fix/final-excalidraw-08.txt` (SHA256
  `29c705be70c8ac4cdc3270f8ec0934cfcb3692a78dcdb6e9c9d899b840fa0a66`).
  Do not claim byte-identical output or semantic equivalence from this timing.

## Final source state

- Caller probe SHA256 after once-per-snapshot edge graph and local alias fix:
  `b1bbc61fefd4cbfbc1252f595c6e604bef368b5f8b2cf7d4dd4ec51372722ebb`.
- Tests SHA256: `4b1a00d911da2ecb1a27d594ed86341e714e9b7e2b0012067bea9d041d380094`.
- Verification on final source: 27 tests passed, 11 subtests passed; Python
  compilation and `git diff --check` passed. The final source was not timed
  again; final native replay/source review must assess delivered output.
