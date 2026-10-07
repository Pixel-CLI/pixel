# `pixel review-gate` — deterministic pre-review findings on the code graph (design note)

Status: issue #537 proposed a finding vocabulary (`{rule, severity, file, line,
evidence, fix_hint}`), a witness epistemics contract (`lower-bound`,
`closed_world: false`) and a rule table for it. Most of the proposal is
already in the tree: `pixel review-gate` shipped with exactly that vocabulary,
and five of the deterministic checks are implemented in
`crates/pixel-graph/src/review.rs`. What is still new is a third of the rule
table (repo-policy checks wired into the gate) and two of the three open
design questions below. This note fixes the vocabulary as the contract, marks
every row of the rule table against the code, and takes a position on the
open questions. Every "today" claim below was read in the code and hooks of
this worktree.

The split that motivates the whole pass: the mechanical half of a code-review
finding (graph + diff + witness) is exactly pixel's territory, and the
semantic half (does this matter, is it more than mechanical) stays model
work. `review-gate` computes what a reviewer would otherwise re-derive from
scratch, one bounded deterministic verdict per change set, and leaves
judgement to a review pass that starts pre-oriented from its output.

## What exists today

```text
pre-push hook ── pixel review-gate . --fail-on concern   (.githooks/pre-push:51)
    │  findings + named caps
    ▼
cr/pixel-graph/src/review.rs          the deterministic pass over the diff
cr/pixel-daemon/src/api.rs            caps → epistemics envelope + RESULT_CAPPED
cr/pixel/src/main.rs                  clap surface, `--fail-on` exit code, pretty print
```

The command, the vocabulary and the envelope are implemented, not sketched:

| Fact | Where | Consequence |
| --- | --- | --- |
| `pixel review-gate [--base] [--fail-on <nit\|suggestion\|concern\|blocker>] [--json]`; exit 1 when the worst finding meets the threshold; no `--fail-on` means "plain review, never block" | `crates/pixel/src/main.rs` (`ReviewGate` clap arm; `review_gate_blocked`, `--fail-on` lines 5525…5537; `ReviewFailOn` with ranks 1..4) | the gate's blocking semantics live on the verdict op, not on a retrieval op |
| the pre-push hook runs it at `--fail-on concern` and refuses the push when it fails | `.githooks/pre-push:42..53` | the deterministic pass already gates the repo's own pushes; a CONCERN or above means `--no-verify` |
| `what-changed` is a retrieval answer: symbols/flows/consumers with `offset`, plus `--tests` mapping changed symbols to test files | `crates/pixel/src/main.rs` (`WhatChanged`, `--tests`; dispatch sends `Request::Changes { include_tests }`); `crates/pixel-graph/src/changes.rs` (`ChangesReport`) | the issue's question "new op vs `what-changed --review`" is already settled in the tree (see Q1) |
| a changed symbol whose callers were not themselves changed — the rule the issue calls `signature-changed-callers-stale` / `producer-reader-divergence` — reads `store.edges_to(uid, Calls)` and filters callers whose file is in the change set | `review.rs` `consumers_outside_change` | one finding covers both rows of the issue's table (see the rule table) |
| added lines are scanned for credential-shaped content; the matched value is never echoed, only the pattern class | `review.rs` `added_secret_findings`, `SECRET_VALUE_PATTERNS`, `SECRET_NAME_ASSIGN`, `has_aws_access_key` | `possible-secret` exists today, despite the issue marking secret-in-diff as ➕ new |
| the report's completeness signals become findings and named caps, never silence | `review.rs` `graph_findings`, `ReviewReport.caps`; `crates/pixel-daemon/src/api.rs` `derive_epistemics` (`basis` = `"code graph + working-tree diff"` for `changes`/`review_gate`, every cap a `RESULT_CAPPED` warning) | the witness contract below is already wired end to end |

## Finding vocabulary and the witness contract

A finding is one rule that fired, plus the anchor and the witness that
reconstructs it. The struct IS the issue's vocabulary, field for field:

```rust
pub struct ReviewFinding {
    pub rule: String,          // the machine name, e.g. "producer-reader-divergence"
    pub severity: String,      // stored: "LOW" | "MEDIUM" | "HIGH" | "CRITICAL"
    pub file: Option<String>,  // repo-relative path; None = change-set-level
    pub line: Option<u32>,     // 1-based working-tree line; None when the rule has no line
    pub evidence: String,      // the witness: enough text to reconstruct the finding
    pub fix_hint: &'static str // the one-liner a reviewer should act on
}
```

`crates/pixel-graph/src/review.rs` defines it; the CLI prints exactly these
six fields, never derived state (`pretty_review_gate` in `main.rs`): severity
in the review vocabulary, `file:line` anchor, rule, evidence on an indented
line, `fix: <hint>` on the next. `--json` emits the struct as serialized.

**Severity is a review word on top of a stored rank.** The stored four
(`LOW`..`CRITICAL`) render as `NIT`..`BLOCKER` (`severity_label`), and rank
1..4 (`severity_rank`) is what `--fail-on concern` compares against: HIGH (3)
or above fails the pre-push gate. An unknown severity is never relabelled —
it prints as it came, so a rule somebody forgets to rank cannot silently
mask as a NIT.

**The witness epistemics contract is `lower-bound`, `closed_world: false`.**
This is the same contract `what-changed` already speaks and the daemon stamps
on every graph answer:

- `pixel_proto::Epistemics` defaults to `{closed_world: false, lower_bound: true}`; a producer must establish a closed world with source-native evidence, and graph answers never try (`crates/pixel-proto/src/epistemics.rs`: default, and the `extraction_limits` field whose docs say an absence is bounded).
- `derive_epistemics` sets `closed_world` only when *both* `caps` and `extraction_limits` are empty — and `extraction_limits` is never empty, so a graph-derived answer is **never** closed-world by construction. For `changes` and `review_gate` the `basis` is `"code graph + working-tree diff"`, caps are named verbatim in `basis`, and every cap becomes a `RESULT_CAPPED` warning (`crates/pixel-daemon/src/api.rs`).
- `what-changed` carries the same honesty at the report level: `suggested_tests_lower_bound` (the 100-item cap, line 959 in `changes.rs`), `uncovered_lower_bound` (truncated ranges or an unexamined old side), and the four extraction blind spots in `extraction_limits()`.

The contract in one sentence: **a review-gate finding is "this witness
exists at this anchor" — never "nothing else is wrong"**. "clean — 0
findings" is printed only when the caps array is empty; with any cap fired the
status is "incomplete", and the envelope carries the cap's words so the
consumer (a pre-push hook, a review prompt prefix) can read the degree of
incompleteness without re-running the pass.

## Rule table, marked against the tree

| rule | deterministic check | witness it attaches | status | where |
| --- | --- | --- | --- | --- |
| `changed-symbol-without-test` | `what-changed --tests` → suggested tests: a changed symbol with no suggested test file | changed symbol, no test file among its depth-1 callers | ✅ **exists** — MEDIUM; capped-walk abstention fires a `caps` entry instead of a false "all tested" | `review.rs` `graph_findings`; the walk is `changes.rs` `suggest_tests` (`SUGGESTED_TESTS_CAP = 100`), also what `what-changed --tests` calls (`main.rs` → `Request::Changes { include_tests }`) |
| `signature-changed-callers-stale` | `who-calls` / `impact` over diff symbols: callers of a changed symbol whose own file is not in the change set | producer site + the untouched reader `path:line` | ✅ **exists** — folded with the next row into one finding, `producer-reader-divergence`, MEDIUM | `review.rs` `consumers_outside_change` (the same edge set `impact --direction upstream` reads; capped at 20 consumers per symbol, cap surfaced) |
| `producer-reader-divergence` | `impact --direction downstream` per changed symbol in the issue's wording; the code reads the *reader* relation (callers) | changed producer + un-routed consumer | ✅ **exists** — same finding as above; the code merges the issue's two rows because the stale-caller and un-routed-reader witnesses are the same edge walk | `review.rs` `consumers_outside_change`; DESIGN NOTE: keep the merge — splitting would emit two findings with identical evidence and identical fix |
| `rename-old-name-survives` | `search-content` on the old identifier of a renamed/deleted symbol | old-name occurrences that no new name resolves | ➕ **new as a finding** — the primitives exist (`pixel rename` plans unresolved old-name sites in `rename.rs`; `pixel search-content` greps the old identifier), but review-gate emits no such finding today | design decision in PR; nearest precedent is `unanchored-symbol` (deleted symbol with no anchor, HIGH) |
| `secret-in-diff` | pattern scan over added diff hunks; only *added* ranges, value never echoed | the matched pattern class + anchor (`"added line matches the {class} pattern"`) | ✅ **exists** — `possible-secret`; the issue marks it ➕ new, but it landed in the tree: CRITICAL for a strong value, downgraded a rung in tests/fixtures and for bare literal pattern-table lines | `review.rs` `scan_line_for_secret`, `added_secret_findings` (`SECRET_CAP = 50`, `SECRET_FILES_CAP = 200`, `#[cfg(test)]` region skipped) |
| `repo-policy` | config-driven grep rules over the diff: changelog fragment present, `Closes #<n>` in the PR body, version bump accompanied by an extractor/bump commit | the diff line or commit that violates the policy | ➕ **new** — nothing in review-gate checks these. The *policies* exist as repo rules, not as checks: CONTRIBUTING.md requires a `changelog.d/<slug>.<section>.md` fragment (enforced at the release cut by the release skill's `prepare.sh --check`, as CI runs it); `.agents/rules/project-task.md` requires `Closes #<number>` as the first PR body line; version-consistency checks are `pixel check-release` (`crates/pixel-release/`). Wiring each as a `repo-policy` grep over the diff is the unbuilt half | design decision in PR |
| `logic` / `security` / `design` | — | — | **model only, by design** | the deterministic pass stops at "this witness exists"; judgement that a data race is exploitable, or that an abstraction is wrong, is the model's |

Four rules beyond the issue's table are also in the tree, all change-set
level and all carrying their witness: `risk-climb` (change-set risk reached
HIGH/CRITICAL → MEDIUM, a SUGGESTION that no edit of the change can clear),
`uncovered-change` (a changed range maps to no symbol; LOW for a non-text
change, MEDIUM otherwise), `unanchored-symbol` (a deleted symbol with no
anchor in the current graph → HIGH), and `unresolved-callers-lower-bound`
(the `detect` envelope note's `"lower bound:"` marker → MEDIUM). Findings are
sorted CRITICAL-first and truncated at `MAX_FINDINGS = 200`; the truncation
is a cap, named, never silent.

## Open design questions

**Q1 — new op `pixel review-gate`, or `what-changed --review`?**
**Recommendation: keep the new op (already the shipped shape).** `what-changed`
is a retrieval answer — symbols, flows, consumers, test files, paged with
`offset` — while review-gate is a verdict with findings, a review severity
vocabulary, and an exit-code contract the pre-push hook relies on. Bolting a
blocking gate onto the `changes` request would leak gate semantics into a
retrieval contract and force every `what-changed --json` consumer to ignore
the new fields. The two stay one diff-read: review-gate reuses
`changes::detect_diffs` for the graph side and adds its own walks, so the
split costs nothing at runtime. (On per-repo rule sets via `run-recipe`: no.
`run-recipe` compiles intent → bounded *retrieval* recipe; review-gate is a
fixed verdict surface with an exit code. If repos want a custom rule set it
extends the gate's `rule` namespace from config and stays a verdict.)

**Q2 — emit review-gate output as a review-prompt prefix ("harness computes,
model judges")?**
**Recommendation: yes, and it is the primary consumer of the vocabulary.** The
six-field finding with its witness is already shaped as the context a review
pass starts from — the rule name, the anchor, the evidence, the fix hint — and
the clap docstring promises exactly that ("each finding carrying the witness
that established it. Feed the output to a real review as the narrowed context
it starts from"). `--json` is the machine-addressable rendering of the same
findings; a prompt hook can prepend the `--json` array plus the
`caps`/`basis` line so the model starts pre-oriented on *what the harness
already proved*, and spends its budget on the semantic half. No new op: the
human CLI rendering and the prompt prefix are two projections of the one
report.

**Q3 — severity mapping from witnesses?**
**Recommendation: tie severity to witness strength, not to the rule.** A stale
caller *with an unresolved site* is HIGH, because that caller's witness is a
claim the graph cannot refute (same-name site, dynamic dispatch, callback) —
a silent behavioral fork is possible, the same reasoning that already makes
`unanchored-symbol` HIGH. A resolved, edge-backed stale caller stays MEDIUM:
the graph *did* see the reader, so "route it or record why not" is a
suggestion, not a concern. A missing changelog fragment is LOW — a compliance
gap with no correctness claim, and it must never trip `--fail-on concern`
(the pre-push gate). Today the code is coarser: `producer-reader-divergence`
is flat MEDIUM and `unresolved-callers-lower-bound` is MEDIUM separately; the
change, if made, walks `edges_to(…, Calls)` and the unresolved-same-name
sites and marks the intersection HIGH. `possible-secret` already models
witness strength (strong value CRITICAL, fixture/bare-literal downgraded) —
the pattern to generalise.

## Known limits

These are bounded truths of the pass, surfaced as caps, never fixed by more
heuristics — "I say when I don't know" is the point.

- **Graph coverage is lower-bound.** Callbacks passed as arguments, dynamic
  dispatch, macro-generated calls, and `eval`/`new Function` are invisible to
  tree-sitter extraction. Every graph answer carries these four strings in
  `extraction_limits` (a daemon test asserts all four are present), so
  `closed_world` stays false and an absence — "no callers", "no findings" —
  is read as bounded. `review-gate` inherits it through `detect_diffs`.
- **Rust in-file `#[test]` fns and `#[cfg(test)]` modules are not in the
  graph** (extraction skips test containers). `suggest_tests` therefore
  cannot suggest an in-file unit test, so `changed-symbol-without-test`
  fires where one exists — a false positive by design, bounded by
  `suggested_tests_note` ("Rust #[test] functions … are not in the graph"),
  and by the 100-item cap that abstains rather than claims.
- **Uncovered ranges are lower-bound too**: `scan_uncovered` reports a
  change that maps to no symbol only down to what it examined; a truncated
  list or an unexamined old side flips `uncovered_lower_bound` and a `caps`
  entry, never silence.
- **Output caps are named, never silent**: consumer findings capped at 20
  per changed symbol, secrets at 50 (files at 200), findings at 200 — each
  becomes a `caps` entry, a `basis` fragment in the envelope, and a
  `RESULT_CAPPED` warning.
- **What the pass is not**: it does not judge logic, security exploitability,
  or design. A clean report is "0 findings found with these bounded eyes",
  and the envelope says so every time.