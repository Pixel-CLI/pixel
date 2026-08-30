# pixel — Scenario Coverage (SUPERSEDED audit + current status)

> **This document's original body audited the tree at commit `9d707dd` ("seed pixel from the
> gitpixel workspace") — 23 commits behind current HEAD (`ccb9c72`). Its central claims are no
> longer true and it has been superseded.** The original text stated that `pixel-ops`,
> `pixel-rank`, and `pixel-facts` did not exist, that there was no `resolve`, `excavate`, or
> `reconcile` op, and that scenario 3 coverage was 0%. All of that has since been built. The
> authoritative, maintained descriptions of scenario behavior now live in:
>
> - the pixel rule — `~/.agent-config/rules/pixel.md` (five scenarios, tenets, three-outcome contract)
> - `README.md` (feature surface, measured performance incl. regressions, agent-tool support)
> - `PLAN.md` §A0/§A2 (doctrine and op surface)

## Current status (verified against HEAD `ccb9c72`, 2026-08-30)

Facts checked directly this pass (crate listing + `pixel <op> --help` against the binary):

- `crates/` now contains `pixel-ops`, `pixel-rank`, `pixel-facts`, and `pixel-install` in
  addition to the seeded crates — the three crates the old audit reported missing all exist.
- `resolve`, `excavate` (with `--phrase`/`--file`/`--from`/`--to`/`--show`), `reconcile`
  (`--strategy report|rebase-if-clean`, `--push auto|none`), `history-search`, and the full
  mutation surface (`publish`/`push`/`ship`/`branch`/`update`/`sync`) are real, accepted CLI
  subcommands.
- The scenario set is now **five**: recovery (`rescue`/`excavate`), resolution (`resolve`),
  branch sync (`reconcile`), task scoping (`targets` — mandatory first call, **advisory** list:
  the hard read-fence was dropped after `docs/bench/sniper-discovery.md` measured it collapsing
  gold-file recall 0.60 → 0.19), and blast radius (`impact`/`changes`).

## What remains honestly open

Per tenet T1, coverage percentages are not restated here without a fresh measured audit. The
known open items carried forward from the old audit that are still true:

- `targets` can only name files that already exist — files a task must CREATE are structurally
  outside candidate generation (one reason the read-fence was demoted to advisory).
- Agent-level A/B (`pixel-bench-results.txt`, 2026-08-30) shows s2-scope and s4-recover
  currently **worse** than the no-pixel baseline; fixes in flight target those regressions, and
  per tenet T3 those scenarios' MANDATORY status depends on a non-inferior re-measurement.

Anything else claimed about per-scenario coverage should be re-derived from the current tree,
not from this file's history.
