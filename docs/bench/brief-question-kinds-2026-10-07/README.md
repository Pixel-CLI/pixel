# Brief experiment evidence — 2026-10-07

See [the report](../../brief-question-kinds-experiment.md) for conclusions and
measurement boundaries. All JSON is UTF-8; schema versions are local to this
experiment.

- `historical.json`: 18 answer rows, nine delivered-hook receipts, exact
  context blocks, original transcript hashes, final answers and scenario rubrics.
- `skill-a1.json` / `skill-b1.json`: 18 / 12 answer rows, final answers,
  settings, scenario rubrics and paired statistics. A has two partial pattern
  scores; B has six pairs, not nine.
- `fresh-probes.json`: 48 source-aware retrieval trials / 57 invocations
  against prepared source/binary `8fe6e10`. Resolve `${PIXEL_BIN}` and
  `${SNAPSHOT}` in argv. Streams are deduplicated by SHA-256 of normalized
  text. Preparation cost is separate; no model was run for these eight cases.
- `expected-facts.json`: human facts and false-claim ledger for
  `eval/scenarios/qk-*.json`. Impact includes tests; these are topical
  questions, not eight mutually exclusive classifier labels.
- `classify.json`: nine human-expected/winning-route comparisons. Case tuples
  are `[prompt, expected, predicted, winner_probability, confidence]`.
  Full distributions and usage costs were not retained.
- `packet-c1.json`: 12 answers, three arms on two lookup questions; identical
  custom-packed evidence in the two packet arms, charged query/packing/model
  timings, final answers, source and runner provenance.

Raw arena transcripts remain in the local run directories named by the
receipts. Hashes identify those originals; they do not make the raw files
public. Published final answers/rubrics allow checking the report's semantic
claims without publishing authentication, complete environments, or host paths.

The original foreign fixture revision is
`5c47874700c23a6c9e976de3f553ffe75bac39d8`; its source must be available to
replay the live trials. The source scenarios are included in this PR.
The local Pixel-source probe and the foreign-repository live trials are
different experiments and must not be pooled.
