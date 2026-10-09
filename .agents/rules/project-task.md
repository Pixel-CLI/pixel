# A Task Exists Before Work Starts

Always loaded: every unit of work in this repository is tracked by an issue
on the project board, and the pull request that does the work links that
issue the way GitHub itself understands.

- **The task comes first.** Before starting any work — a fix, a feature, a
  rule, a docs pass — check [project 3, view 1](https://github.com/users/LivioGama/projects/3/views/1)
  and the repository's issues for one covering it. When none exists, open
  one (`gh issue create --title "<what and why>"`) and, if you can, add it
  to the project (`gh project item-add 3 --owner LivioGama --url
  <issue-url>`); a contributor outside the organization cannot, and a
  maintainer adds it. A task-out-of-band conversation note is not tracking.
- **The PR body opens with `Closes #<number>`.** That closing keyword (or
  `Fixes` / `Resolves`) links the pull request to the issue: GitHub lists it
  under the issue's Development section and closes the issue when the PR
  merges, and `board-sync.yml` reads the same link. A PR that does only part
  of an issue writes `Refs #<number>` instead, so the issue stays open; one
  that closes several lists each (`Closes #1, closes #2`). The branch,
  commit subjects and the `changelog.d/` fragment may name the issue too;
  the pull request is one artefact of the task, not the task itself.
- **The board follows the PR lifecycle, automatically.** The `Status` field
  on project 3 of every issue the PR closes is set from the pull request
  state by `.github/workflows/board-sync.yml`, never by hand:
  - **In Progress** while the PR is open (at open, reopen, and an edit that
    adds the closing keyword).
  - **Done** when the PR merges — and only then. A green CI alone is not
    Done; a merged PR is.
  - **Todo** when the PR is closed unmerged, so the board never shows
    finished work that does not exist.
  The job reads GitHub's `closingIssuesReferences`, not the body text, so a
  keyword GitHub did not link (inside a code span, a typo, an issue number
  with no issue) moves nothing: check the issue's Development section after
  opening the PR.
- **Keep the board honest while working.** No status may outrun the PR:
  an agent never moves an item to Done as it pushes, and a status set by
  hand ahead of the PR is a claim nothing supports. When a task turns out
  bigger than its issue says, stop and split the issue before continuing.
- **No issue is the exception.** A one-line typo fix, a CI rerun or an
  emergency revert may go without an issue; its PR body then has no closing
  keyword and says in a line why no issue was needed. Everything else links
  its issue as above.

## Install side-build & remote smoke (item #4, sub-task openai-install-smoke)

Recorded procedure, verified live on 2026-10-07:

1. `pixel self-update --dev --repo . --build "cargo build --profile
   dev-release -p pixel-cli"` — green (~1.2s rebuild, installs
   `target/dev-release/pixel` to `~/.local/bin/pixel-dev`).
2. `pixel-dev build-index --history .` (fresh, 2413 commits, 100% diff
   coverage) then `pixel-dev install --repo .` — 8 green, 0 yellow, 0 red.
3. Enter the key at a non-echoing prompt, then pipe it to Pixel's config
   command (the key is neither a command argument nor printed back):

   ```sh
   read -rs 'openai_key?OpenAI API key: '
   printf '\n'
   printf '%s' "$openai_key" | pixel-dev config remote-key openai -
   unset openai_key
   ```

   This writes the key to `~/.pixel/config.yaml` (never the repo).
4. Live smoke: `pixel-dev classify 'Choose the letter that comes first in
   the alphabet.' --context 'Classify the text.' --label a --label b
   --remote-preset openai --engine remote --json` returned
   `probs {a:0.94, b:0.06}`, `snapshot.provider=openai`,
   `model=gpt-6-luna` (the openai preset's default model).

Findings from the smoke (candidate edits live in `crates/`, not here):

- The `openai` preset posts to `https://api.openai.com/v1/decisions`
  (OpenAI's Decisions shape), not `/chat/completions`; a Decisions
  `refusal` answer surfaces as
  `remote decisions refused the decision: (no reason given)`.
- The key itself is valid (`GET /v1/models` → 200; chat completions
  work), but the account has Decisions access only on `gpt-6-luna`:
  every other model id (gpt-4o, gpt-5, gpt-5.1, gpt-6.1-sol, …)
  returns 404 `model_not_found` on `/v1/decisions`.
- Vague input (`'smoke test'` + context `t`) makes `gpt-6-luna` return a
  Decisions refusal with no reason; a well-posed question returns a
  proper `probabilities` distribution. Consider a clearer error hint
  (input too vague / model lacks Decisions access) in
  `crates/pixel/src/decide_remote.rs`.
5. `pixel-dev doctor . --fix --fail-on yellow --skip 'install.*' --skip
   repo.codex-hook-review` — 17 checks ran, 17 green, 0 yellow, 0 red.
