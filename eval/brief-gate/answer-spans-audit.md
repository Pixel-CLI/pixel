# Answer-span audit

`answer_spans.jsonl` was audited against the pinned brief-gate fixture at
`85bede7d9a3c4e385e0a2045241a6466efbd8cbd`. This is a source/provenance audit;
retrieval scores were not used to select or change any answer span.

The audit preserves all 33 annotated questions: 28 rows are unchanged, three
rows remap stale paths to equivalent source in the pinned fixture, and two rows
add missing call-site/control-flow evidence. No question was removed or
relabelled. The corrected file has 45 spans across 33 unique question IDs.

| ID | Audit status | Evidence / reason |
| --- | --- | --- |
| bg-006 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-014 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-015 | path remapped | `exclude_locally` is in `crates/pixel-install/src/repo_git.rs`; the old `pixel-setup` path is absent in this fixture. The module documentation and function remain relevant. |
| bg-018 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-030 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-032 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-036 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-037 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-070 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-073 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-074 | expanded | Kept the `code_signal` definition and added its retrieval-request and `Request::from_prompt`/`start_with` call sites, which answer how it participates in brief retrieval. |
| bg-085 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-086 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-093 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-094 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-095 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-104 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-119 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-122 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-127 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-128 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-120 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-136 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-146 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-149 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-161 | path remapped | `select_range` is in `scripts/mutants-nightly-range.py:31-72`; the former `scripts/ci/` path is absent in this fixture. |
| bg-163 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-178 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-180 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-181 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-203 | expanded | Kept the trivial-continuation classifier and added `chain.rs:1258-1266`, where `start_with` returns early for that classification and skips the brief. |
| bg-209 | unchanged | Existing annotated behavior remains present in the fixture. |
| bg-211 | path remapped | `merge_hook_entry` is in `crates/pixel-install/src/config.rs:533-564`; the former `pixel-setup` path is absent in this fixture. |

The original SHA-256 was
`2704efba4f1a34f97f08dc263930b221cf89df8023f6bbff28fde384dd3161c7`; the
audited SHA-256 is
`2d1eeafc70b873a3f8c6dd406b5685a3e2bb11f5ab94a2fd8c3e533e13d2a4f6`.
Validation checked 33 unique IDs and all 45 spans for source-path existence and
line bounds in the pinned fixture; it reported zero issues.

These annotations remain a curated reference set, not exhaustive relevance
judgments. The audit establishes that the marked lines exist and are supported
by the associated prompt; it does not claim that unmarked source lines are
irrelevant.
