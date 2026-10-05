# scripts/

Every script runs from the repo root. Shell scripts find `pixel` the same
way: `$PIXEL_BIN`, else the installed binary (`command -v pixel`, a
mise/asdf shim, Homebrew or `~/.cargo/bin`), else `target/dev-release/pixel`
then `target/release/pixel`. Install a build that matches the tree with

```bash
pixel self-update --repo . --build "cargo build --profile dev-release -p pixel-cli"
```

(never `cp` into `~/.local/bin` by hand; see CONTRIBUTING.md "Optional local install verification").

## Gates and contracts (CI runs these)

| Script | Run | What |
| --- | --- | --- |
| `gates.sh` | `scripts/gates.sh [--force]` | fmt, clippy, nextest/test with laptop-safe defaults; exits 0 without compiling when nothing Rust-affecting changed |
| `codeql-rust-scope.py` | called by CodeQL CI | omit Rust only for known non-Rust PR merge diffs; unknown inputs and non-PR events scan fully |
| `test-codeql-rust-scope.py` | `python3 scripts/test-codeql-rust-scope.py` | changed/deleted/renamed Rust, shallow merge diffs, policy edits and full scheduled scans |
| `coverage-nightly.py` | called by Coverage CI | skip instrumentation only after a successful scheduled measurement of the same main SHA |
| `test-coverage-nightly.py` | `python3 scripts/test-coverage-nightly.py` | unchanged main, failures and scheduled-only coverage |
| `test-pre-push.sh` | `sh scripts/test-pre-push.sh` | publication starts no local validation or fetch |
| `test-gates.py` | `python3 scripts/test-gates.py` | contract of `gates.sh` (stub cargo in a throwaway repo) |
| `mutants-gate.py` | `python3 scripts/mutants-gate.py --diff pr.diff --list mutants-list.txt` | the `Mutants` plan/report script; sizes the shard matrix and deals each shard a runner from the `PIXEL_MUTANTS_SHARD_RUNNERS` pool (JSON array of `runs-on` values, round-robin; GitHub-hosted when unset) |
| `test-prepare.py` | `python3 scripts/test-prepare.py` | contract of `.agents/skills/release/prepare.sh`'s pull request listing (stub gh/cargo, disposable repo, needs `jq`) |
| `test-install.py` | `python3 scripts/test-install.py` | contract of `install.sh` (fake curl/uname, local tarball) |
| `test-clean.py` | `python3 scripts/test-clean.py` | contract of `clean.sh`, mostly what it must *not* remove (disposable repo with a second worktree) |
| `test-pr-swarm.py` | `python3 scripts/test-pr-swarm.py` | contract of `pr-swarm.sh` (stub `gh`/`rmux`/`claude`, real git in a throwaway repo): idempotency, a `gh` outage never tearing down, merge/teardown rails, branch-rename retitling and agent-name derivation |
| `verify-action-pins.py` | `python3 scripts/verify-action-pins.py` | every `uses:` in `.github/` pinned to a full commit SHA with a `# <ref>` comment (or local, or a docker digest); unparsed forms fail closed |
| `test-verify-action-pins.py` | `python3 scripts/test-verify-action-pins.py` | contract of `verify-action-pins.py`: tags, short SHAs, missing comments and quoted or flow forms refused, this repository's workflows accepted |
| `cancel-stale-runs.sh` | `scripts/cancel-stale-runs.sh [--branch NAME \| --closed \| --all] [--older-than MINUTES] [--apply]` | prints, or with `--apply` cancels, queued and in-progress runs: `--branch NAME` those of one head branch (`cancel-stale.yml` passes the closed pull request's head), `--closed` pull-request runs whose branch has no open pull request (`cancel-stale.yml`, every ten minutes), and `--all` every run, `main` pushes included, as a manual backlog purge (`--older-than` limits it). In every mode it spares `pull_request_target` runs (the board sync) and its own run; `test-cancel-stale-runs.py` holds that |
| `check-advisory-ignores.py` | `python3 scripts/check-advisory-ignores.py` | `deny.toml`'s `[advisories] ignore` and `osv-scanner.toml`'s `[[IgnoredVulns]]` (read by OpenSSF Scorecard) name the same advisories, every entry in either file once and with a reason; run by the `deny` CI job |
| `test-check-advisory-ignores.py` | `python3 scripts/test-check-advisory-ignores.py` | contract of `check-advisory-ignores.py`: an acceptance missing from either file, an entry without a reason (deny.toml's bare-string form included) or a duplicate id fails, this repository's two files pass |
| `install.sh` | `curl -fsSL https://github.com/Pixel-CLI/pixel/releases/latest/download/install.sh \| sh` | end-user installer, published as an asset of every release: latest GitHub release, checksum, runs the binary, atomic rename into `$PIXEL_INSTALL_DIR` (default `~/.local/bin`), appends that directory to `$GITHUB_PATH` when set |
| `homebrew-formula.py` | `python3 scripts/homebrew-formula.py <tag> <artifacts> <out>` | run by `release-build.yml`: writes the tap's `pixel.rb` and the two Linux bottles (the musl binary as a keg, reproducible) from the release archives |
| `test-homebrew-formula.py` | `python3 scripts/test-homebrew-formula.py` | contract of `homebrew-formula.py` (synthetic archives, `fixtures/homebrew/pixel.rb.template`) |
| `release-sbom.py` | `python3 scripts/release-sbom.py <raw.cdx.json> <cargo-tree.txt> <tag> <target> <out>` | run by `release-build.yml`: narrows cargo-cyclonedx's SBOM of the `pixel` binary to the crates `cargo tree -p pixel-cli` compiles for that target and feature set, refusing a compiled crate the SBOM misses or an SBOM of another version or target |
| `test-release-sbom.py` | `python3 scripts/test-release-sbom.py` | contract of `release-sbom.py` (synthetic cargo-cyclonedx output and `cargo tree` listing) |
| `homebrew-core-formula.py` | `python3 scripts/homebrew-core-formula.py <tag> <source-tarball> <out>` | run by `release-build.yml`: writes `pixel-core.rb`, the homebrew-core formula that builds pixel from the tag's source archive (see `docs/homebrew-core.md`) |
| `test-homebrew-core-formula.py` | `python3 scripts/test-homebrew-core-formula.py` | contract of `homebrew-core-formula.py` (`fixtures/homebrew/pixel-core.rb.template`); `.github/workflows/homebrew-core.yml` builds and audits the formula |
| `refresh-guidelines.sh` | `scripts/refresh-guidelines.sh` | re-download the vendored Rust guidelines; exit 1 when rule headings moved |

## Disk

| Script | Run | What |
| --- | --- | --- |
| `clean.sh` | `scripts/clean.sh [build\|index\|cache\|bench\|all] [--dry-run]` | reclaim what this checkout can rebuild, across every worktree `git worktree list` reports. `--help` documents each scope and what getting it back costs |

The `justfile` is a front end for it: `just disk` is `clean.sh all --dry-run`,
`just clean` is the build scope, `just clean-index` / `clean-cache` /
`clean-bench` / `clean-all` are the others. Start with `just disk`: it prints
the exact list the other recipes would remove and removes nothing. The recall
corpus and `~/.local/state/pixel` are never touched by any of them.

## Worktrees and panes

| Script | Run | What |
| --- | --- | --- |
| `pr-swarm.sh` | `scripts/pr-swarm.sh reconcile [--wait N\|--no-wait] [--dry-run] \| status \| up <PR> [--worktree] \| down <PR> [--force] \| watch \| hook-session-start` | one rmux pane per open-PR worktree, each a `claude -n pr-<N>-<slug>` session sitting in that PR's tree; `reconcile` diffs the open PRs against panes titled `PR#<N>` and creates, retitles or tears down (`status` is read-only). Wired to SessionStart by `.claude/settings.json`; rails in `.agents/rules/pr-swarm.md` |

## Optional local smoke and audits

| Script | Run | What |
| --- | --- | --- |
| `pixel-smoke-test.sh` | `scripts/pixel-smoke-test.sh` | the installed binary end to end: `--version`, the guard hook's advisory/rewrite/passthrough contract across Claude, Devin, Codex and Gemini tool names, session-start, `doctor --json`, the install surface, help of the mandatory workflows. Read-only. `PIXEL_SHELL=fish` when `doctor` must check another shell's wrapper |
| `system_audit.py` | `python3 scripts/system_audit.py --pixel target/dev-release/pixel --output /tmp/audit.json` | every CLI leaf against a disposable repo and `$HOME` (retrieval, mutations, env, sniper, hooks, tasks); JSON report, exit 1 on any FAIL. About one minute |
| `system_audit_recall.py` | `python3 scripts/system_audit_recall.py target/dev-release/pixel --output /tmp/recall-audit.json` | recall index/search/ask/export, both daemons, install/uninstall/doctor/migrate/upgrade in a disposable `$HOME`; no network, no model download |

Both audits set `PIXEL_DAEMON_AUTO_START=0` and clean up after themselves;
`pgrep -fl "pixel daemon"` afterwards must print nothing.

## Demos and benches (need a terminal; the benches need `claude` logged in)

| Script | Run | What |
| --- | --- | --- |
| `bench-read-savings.sh` | `scripts/bench-read-savings.sh` | the whole file vs `pixel list-signatures` on well-known large files pinned to a commit (bytes ÷ 4); needs `curl` and network, no agent, under a minute. Rows in `website/data/read_savings.toml`, method in `docs/bench/read-savings.md` |
| `problem-trace.py` | `python3 scripts/problem-trace.py > website/data/problem_trace.toml` | the home Problem chapter trace: the median-call run of the `vanilla` arm archived in `docs/bench/problem-trace/` (recorded with `docs/motion/scripts/record-demo.sh`), commands shortened, each call classed as search, read or other. Method and both arms in `docs/bench/problem-trace.md` |
| `scope-packet.sh` | `scripts/scope-packet.sh > docs/bench/problem-trace/packet.txt` | reproduces the task packet the `pixel` arm of that recording received: rebuilds the recorded repository like `record-demo.sh` and feeds the recorded prompt to the prompt-submit hook; refuses any binary but the recorded one (`meta.txt`) |
| `scope-board.py` | `python3 scripts/scope-board.py > website/data/scope.toml` | the home Scoping board: the packet's P0/P1 files with their positions in the recorded tree, the index size, and the files every `pixel` run named that the packet left out. Method in `docs/bench/problem-trace.md`, "The Scoping board" |
| `pixel-vs-manual.sh` | `scripts/pixel-vs-manual.sh [repo]` | five retrieval tasks, grep/git vs pixel, timings side by side; no agent. Indexes the repo on first run |
| `pixel-excavate-demo.sh` | `scripts/pixel-excavate-demo.sh /path/to/repo` (`PHRASE=…`) | history archaeology, `git log -S` vs `pixel dig-history`; the repo needs `pixel build-index --history` once |
| `pixel-demo.sh` | `SCENARIO=scope scripts/pixel-demo.sh [repo]` | one `claude -p` scenario, baseline (`--safe-mode`, pixel hooks stripped) vs pixel; a few minutes |
| `pixel-bench.sh` | `N=3 scripts/pixel-bench.sh [repo]` | the 4-scenario A/B matrix; 10 to 40 minutes, results in `docs/bench/pixel-bench-results.txt` |
| `pixel-bench-isolated.sh` | `scripts/pixel-bench-isolated.sh [N]` | pixel's doctrine alone vs a blank agent, both under `--safe-mode`; run `pixel-bench.sh` once first (it writes the prompt files) |
| `harness-recorder.sh` | `HARNESS_OUTDIR=target/recordings scripts/harness-recorder.sh --provider claude --scenario scope --gif [--post PR]` | record a harness run (Claude Code or Codex, pixel arm wired like `pixel-demo.sh`) under `asciinema rec`: `harness-<provider>-<scenario>.cast` + `.txt` transcript + `.gif` (agg) + `meta.json`, then `--post` a PR comment with the stats table and transcript and a gist of the `.cast` for replay. Scenarios are the four demo prompts plus `rns` (the harness retrieval smoke checklist). `--upload` posts the `.cast` to an asciinema server you have authenticated against (`asciinema auth`). Needs `asciinema` and `python3` (`--gif` needs `agg`: `brew install asciinema agg`); `gh` only with `--post` |
| `test-harness-recorder.py` | `python3 scripts/test-harness-recorder.py` | contract of `harness-recorder.sh` (stub asciinema/agg/gh/claude/codex: cast header, transcript replay, pixel-call counting, PR-comment body, gh-less `--post` refuses) |
| `codex-pixel-ab.sh` | `scripts/codex-pixel-ab.sh --prompt 'Does Pixel help with this repository question?' [--pixel-policy advisory\|enforce\|classify]` | opens an unattended tmux A/B run in two isolated worktrees at one committed SHA: raw Codex (`--ignore-user-config`, rules and hooks off) beside Codex with a project-local candidate Pixel install. `enforce` recreates the historical PreToolUse guard experiment only in the Pixel arm; `classify` saves a three-way routing decision and enables enforcement only when Pixel wins its `--classify-min-confidence` threshold (default 0.60), otherwise runs a clean control-equivalent arm. The panes end on `report.md`, which records policy, classifier evidence, exits, wall time, transcript Pixel retrieval commands, prompt/base hashes and answer files. Requires `codex` already authenticated, tmux and a candidate `pixel`; local source changes are excluded with a visible warning and live model use may edit only disposable worktrees. |
| `test-codex-pixel-ab.ts` | `bun scripts/test-codex-pixel-ab.ts` | wiring contract of `codex-pixel-ab.sh` with stub tmux/Codex/Pixel and a temporary committed git repository; asserts a paired report is produced without a live model. |

The pixel arm of the `claude -p` benches is given the deployed agent prompt
(`~/.local/share/pixel/agent-prompt.md`, the bundled
`crates/pixel-install/assets/pixel-agent-prompt.md` when nothing is
installed) through `--append-system-prompt-file`, exactly as the `claude`
shell wrapper written by `pixel install` does. The scripts call `claude` by
path, so a fish/zsh wrapper function never applies to them; without the
flag the "with pixel" arm would run without pixel's instructions.

`mutants-nightly-range.py` selects the cumulative main diff from completed campaign checkpoint metadata and writes a checkpoint only for fully judged outcomes. `test-mutants-nightly-range.py` tests replay, no-change skips, and checkpoint integrity.
