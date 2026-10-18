# The Ruby hybrid's correctness, reassessed on main (2026-10-18)

Closes #1205 (child of #1040, unit (c)). This report reassesses
Pixel-CLI/pixel-mvp#111 and the correctness half of ADR-0037 §5 on main
`904d9570`. It compares the hybrid graph (`PIXEL_GRAPH_SOURCE_RUBY=hybrid`)
with v1's own Ruby graph on antiwork/gumroad `b996417e`. No agent ran a
task: agents only judged the sampled edges against the Ruby source.

## Verdict

**The correctness bar holds.** Over every merge row judged, the merge
wrote no wrong target:

- the 75 edges it dropped were all wrong;
- a sample of 80 of the 11 005 edges it added: 80 right;
- a sample of 40 of the 6 239 edges it raised to `exact`: 40 right;
- a sample of 40 of the 86 routes it added: 40 right.

The hybrid's estimated correct call edges are 107 544, against v1's 96 539,
and its call precision is 62.6 %, against v1's 60.0 %. ADR-0037 §5 asks for
both to be at least v1's.

Two defects remain. Neither is a wrong target.

- **6 merge edges sit on a caller that is no symbol.** Those sites read as
  vanished. This is v1's own defect: same-named definitions in one file
  overwrite each other's `symbols` row (#1217). v1 already holds 73 such
  edges and 111 such unresolved rows on gumroad.
- **One #111 case is still open.** pixel-mvp proves no self-call through
  gumroad's `ApplicationController`. This is a recall loss: the hybrid keeps
  v1's edges at those sites (#1218).

The default stays off for the other units of #1040: brief loss (#1203) and
refresh cost (#1204), measured in `ruby-hybrid-daemon-2026-10-18.md`.

## Counts, hybrid against v1

Ruby edges from every file v1 reads as Ruby, keyed (caller, callee, site
line) as pixel-research-mvp's `hybridcmp.py` keys them:

| Class | v1 | hybrid | in both | dropped | added | raised to `exact` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| calls with a receiver | 82 868 | 90 387 | 82 850 | 18 | 7 537 | 4 506 |
| calls on `self` | 78 030 | 81 441 | 77 973 | 57 | 3 468 | 1 733 |
| routes | 702 | 788 | 702 | 0 | 86 | 0 |
| associations | 523 | 523 | 523 | 0 | 0 | 0 |
| other references (ancestors, callbacks, …) | 7 528 | 9 190 | 7 528 | 0 | 1 662 | 0 |

The call delta matches 2026-10-16's exactly (pixel-research-mvp
`2026-10-16-ruby-hybrid-performance.md`): 75 dropped, 11 005 added,
6 239 raised, the same counts after the imports since (pixel-mvp
`e91e8da`, #1002, #1043; the edges themselves were not diffed against
2026-10-16's). v1's own count moved: 160 898 edges here, against 160 734.

pixel-mvp itself resolves 53 636 of its 962 240 Ruby call sites, against
2 251 in #111 and 21 807 on 2026-10-15. It also resolves 636 of 925 routes.

## Wrong targets

| Sample | Rows | Right | Wrong | Undecidable | How |
| --- | ---: | ---: | ---: | ---: | --- |
| dropped (v1's edges, replaced) | 75 of 75 | 0 | 75 | 0 | all reused (2026-10-16) |
| added (pixel-mvp's, at sites v1 left unresolved) | 80 of 11 005 | 80 | 0 | 0 | 79 judged, 1 reused |
| raised to `exact` (v1's guess, now proved) | 40 of 6 239 | 40 | 0 | 0 | 40 judged |
| routes added | 40 of 86 | 40 | 0 | 0 | 22 judged, 18 reused |

**Sampling.** Each delta side is sorted by (caller path, line, callee, callee
path, callee line) and drawn with
`random.Random("20261018-correctness-<side>").sample(rows, n)`. A side with
no more than `n` rows is taken whole. Calls with and without a receiver are
drawn together.

**Reused verdicts.** A row reuses a verdict pixel-research-mvp gave to the
same edge (caller path, line, callee path, callee, callee line), taken from
`known.tsv` (its results at `cf6ebd7`).

**Judging.** Every other row was judged by one agent session per file,
following `JUDGING.md` and reading both ends in gumroad at `b996417e`. The
agents were asked to look adversarially for shadowing `def`s in sibling
`describe` blocks, local variables, and gem or Rails methods earlier in the
chain.

Edges pixel-research-mvp had already judged on today's deltas, whatever
sample they came from: 94 of the 11 005 added (all right), 5 of the
raised (all right), 29 of the routes added (all right).

**Estimate.** The formula is
correct(hybrid) = correct(v1) − correct(dropped) + correct(added).

- **v1's level:** its 2026-10-15 random sample of 80 gumroad call edges,
  48/80 right (49.0 to 70.0 %, Wilson). All 80 edges are still in both of
  today's graphs.
- **v1:** 96 539 estimated correct of 160 898 (78 913 to 112 690).
- **Hybrid:** 107 544 of 171 828.
- **Difference:** +11 005 (+10 501 to +11 005). The dropped edges are judged
  whole. The added edges' precision is 80/80 (95.4 to 100 %).

Raising an edge to `exact` changes neither count. It does change what a
reader believes, which is why that sample was judged too.

## Unresolved-site honesty

Every Ruby call site (file, line, name written), as v1 holds it and then as
the hybrid does:

| v1 → hybrid | Sites |
| --- | ---: |
| edge → edge | 160 661 |
| unresolved → edge | 10 971 |
| edge → `unresolved_calls` row (pixel-mvp's target has no v1 symbol) | 57 |
| unresolved → unresolved | 775 474 |
| no site → edge (pixel-mvp's only) | 2 |
| unresolved → **nothing** | **6** |

- **Declared, not silent.** A v1 edge the merge drops where pixel-mvp
  proves a target v1 cannot hold leaves an `unresolved_calls` row: 57 sites.
  `impact`'s lower bound still counts them.
- **The 6 sites that vanish are not abstentions.** At each of them the
  merge wrote pixel-mvp's edge, and the target is right
  (`spec/services/update_user_compliance_info_spec.rb:1032` →
  `UpdateUserComplianceInfo#process`). But the edge's caller is the v1 row's
  `enclosing_symbol_id`, which names no symbol, so every query joining
  `symbols` loses the edge.
- **Cause.** The uid `<path>#<qualified>#<kind>` makes the three
  `def process_with` of that spec one symbol, and `insert_symbol`'s
  `INSERT OR REPLACE` keeps the last of them. v1 has the same defect at its
  own sites: 73 edges and 111 unresolved rows whose caller is no symbol
  (Ruby, TS and TSX). The fix belongs in v1's store, not in the merge:
  #1217, with a minimal reproduction.

## The cases of pixel-mvp#111

| Case (#111) | Now | Evidence |
| --- | --- | --- |
| Ruby call edges: 2 251 against v1's 160 409 | **fixed** | pixel-mvp resolves 53 636 call sites; the hybrid holds 171 828 call edges, against v1's 160 898 |
| routes at 0 (v1: 702) | **fixed** | hybrid 788, v1 702; the 86 added: 40 sampled, 40 right |
| most self-calls abstain: `HealthcheckController`'s chain starts with an unknown step | **open** | 0 self-calls resolve into `ApplicationController`, including 0 of its own 25 and 0 of 745 in `app/controllers/concerns/`. 221 calls from its subclasses to names it defines abstain (`e404` 100, `e404_json` 30, `doorkeeper_token` 20). Controllers' self-calls that stop before it do resolve: 1 253 to their own class, 627 inherited from an intermediate base. Filed as #1218 |
| an unnamed definition on an unknown receiver in app code | **fixed** | no non-test file holds an unnamed `tainted` fact. 8 test files do, and they taint only test callers (#106) |
| an anonymous `Class.new(P)` in app code | **fixed** | 11 unassigned `Class.new` in non-test files; none of those files holds an unnamed taint |

Why `ApplicationController`'s chain is unknown is not diagnosed.
`app/controllers/application_controller.rb` holds only `hidden` facts
(private methods), which act on constant receivers alone. Since even its own
self-calls abstain, the step in question comes before the class itself. A
concern's `included do`, or a hook with a statement the rules do not read,
fits that (D14 in `ancestors.rs`). #1218 records what to look for.

## Pins

| What | Pin |
| --- | --- |
| Pixel | main `904d95709cd9`, `cargo build --profile dev-release --locked -p pixel-cli` (`pixel 0.8.0`) |
| pixel-mvp in Pixel | `e91e8da`, extractors `ruby.14,rust.4`, undo format 2 (`graph_source_ruby` meta) |
| gumroad | `b996417e6fe55b984a8a36b1a2780ef0ca742724`, `git fetch --depth 1`, one APFS copy per arm (`cp -cR`) under `$TMPDIR` (no ignored directory name in the path, #1194) |
| verdicts reused | pixel-research-mvp `cf6ebd7` (`known.tsv`, `v1-sample.tsv`, `judgments/v1-*.tsv`) |
| Machine | Apple Silicon, macOS 27.0 |

## Method

```sh
D=docs/bench/ruby-hybrid-correctness-2026-10-18
sh $D/build.sh <pixel> <v1-clone> <hybrid-clone> <homes>
python3 -I $D/compare.py <v1-clone>/.pixel/graph.v2.db \
  <hybrid-clone>/.pixel/graph.v2.db <hybrid-clone>/.pixel/.pixel-mvp/graph.db \
  <hybrid-clone> $D/runs/gumroad --judgments $D/judgments
```

- **The builds.** `rebuild-graph` took 13 s for v1 and 21 s for the hybrid.
- **The comparison.** `compare.py` prints `runs/gumroad/compare.txt` and
  writes `summary.json` and the samples. It exits 1, for the two open
  defects above.
- **The rest.** `README.md` in the folder lists the files and the sampling.

## What is not verified

- **The judges.** One agent session per file, no second reader. One raised
  edge rests on a convention the guide does not name:
  `InternalNotificationWorker.perform_async` → `#perform`, Sidekiq's
  enqueue-then-run, counted right like ActiveJob's. Counted wrong, the
  raised sample is 39/40, and neither count above changes.
- **v1's precision** rests on 2026-10-15's 80 edges. It sets the level of
  both estimates, not their difference.
- **Samples, not the whole delta.** 80 of 11 005 added edges and 40 of
  6 239 raised: a wrong-target rate under about 5 % (added) or 9 % (raised)
  is not excluded at 95 %.
- **once-campfire** was not rerun, and neither was any repository other
  than gumroad.
- **The hybrid after an incremental update.** Only full builds were
  compared. An update's merge (`merge_again`, being reworked in #1204)
  should store what a full build stores; that is not rechecked here.
- **References other than routes** (1 662 ancestors and callbacks added)
  were not sampled. On 2026-10-15, 20/20 of them were right.
