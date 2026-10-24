# Maintainer runbook

What needs the maintainer's machine, accounts or decisions, with the exact
commands. Nothing here blocks a pull request; each item says what it unlocks.
Numbers from any run go into the pull request or issue with the command and the
run identity beside them (`.agents/rules/measuring.md`).

## 1. Repository settings (once)

| Setting | Where | Unlocks |
|---|---|---|
| GitHub Advanced Security (code scanning) or a public repository | Settings → Code security | `codeql.yml` (the job is skipped while the repository is private) |
| Existing branch protection on `main` | Settings → Branches | Check the candidate's CI names against existing required checks before merge; this PR changes no protection settings |

## 2. Host verification (each host, once per host version)

`specs/hosts.md` marks cells "to verify". For every host you have installed:

```sh
cargo install --path crates/pixel --locked
pixel setup --agent <claude|codex|devin|gemini|antigravity|opencode|pi> --yes
pixel doctor --only 'install.*'
scripts/verify/capture.sh on            # wraps the installed pixel; hooks leave their payloads
# in the host, in a scratch repository: a prompt that names a symbol; have the
# agent edit a file that has callers; let the turn finish
scripts/verify/capture.sh show          # event, stdin payload, stdout envelope, exit code
scripts/verify/capture.sh off
```

For each "to verify" cell of the host the capture must show the event name the
host sent, the payload fields `pixel-setup` reads, and, for an envelope, its effect
in the host (the model's next message cites the injected text; the 🟩 line is
drawn). Then edit `specs/hosts.md`: the cell becomes "measured" with the date and
host version, or the fallback is recorded. **Codex** also needs `/hooks` to review
the new entries before they fire. **pi**: verify the event names and the
`triggerTurn` continue semantics against the installed
`@earendil-works/pi-coding-agent`. The captures hold the typed prompt: read them
before sharing.

## 3. Comparative runs

For optional future trials, use the restored [Arena skill](../.agents/skills/arena/SKILL.md) and explicitly
select a binary built from the candidate. It orchestrates isolated Claude Code
or Codex sessions using the current Pixel commands and hooks; an old-engine
checkout, command shim and the removed benchmark scripts are not required.

Record the prompt set, repository revision, binary SHA-256/version, agent/model
versions and exact commands with tool calls, provider-reported tokens, elapsed
time and answers. Compare answer quality as well as cost. Historical Pixel
figures describe their original samples; they are not an automatic pass/fail
threshold for this engine. Existing comparative evidence stays attached to its
pinned versions; this PR does not require a new benchmark campaign. Verify the
transformation and run a bounded case on its exact published head using the
existing coordinated harness. Release thresholds remain maintainer decisions.
Do not replace published figures with results from an unrelated sample or an
unexecuted candidate run.

## 4. Edit-hooks A/B (decides the default of `edit_hooks`)

Edit hooks ship off. The default turns on only on a correctness or turn gain.
Three arms on an edit task set (a signature change with N callers, a removed
parameter, a renamed field), on Claude Code and Codex: vanilla
(`PIXEL_BRIEF=0 PIXEL_EDIT_HOOKS=0`), brief (`PIXEL_EDIT_HOOKS=0`), brief + edit
(`pixel config --global edit_hooks on`). Measure turns, tool calls, provider-reported
tokens, and whether `cargo build && cargo test` is green at the Stop. Before the
runs, replay real Claude and Codex edit payloads against a ripgrep clone with
`pixel hook pre-edit|post-edit|stop` and check the card names the right callers, the
post-edit message lists exactly the stale sites and the Stop block fires once.

## 4b. Semantic search quality (decides whether `semantic` ships)

The container that built `semantic` could not reach the model host, so only the
plumbing is tested. On your machine:

```sh
cargo build --release -p pixel --features semantic
pixel search-meaning --download            # prints repository, revision, destination first
cd <a repository you know> && pixel prepare
pixel search-meaning "where is the retry limit read from config" --limit 5
```

Judge ten questions you know the answer to: the right file in the first three,
tests and docs below code. Record the question list, the build commit and the
result beside the command (`measuring.md`). The tokenizer stack must also build
for the musl lanes (`cross-build.yml` with `--features semantic`) before the
release build turns the feature on.

## 5. Existing site configuration

The Pixel site is published at <https://pixel-cli.dev/>. This candidate retains
its domain, presentation, legal pages and analytics configuration. The
Cloudflare beacon is configured in `website/hugo.toml`; its existing privacy
disclosure remains. No new palette, domain or hosting provider is selected by
the implementation replacement.

## Publishing the site

The retained `pages.yml` builds `website/`, uploads the `website/public`
artifact and deploys it through the existing GitHub Pages job on relevant
pushes to `main`. A branch candidate's local Hugo build is not a deployment.
The repository's Pages settings and DNS are unchanged. Re-render animations
only with `.agents/rules/readme-webp.md` when their source content changes.

## 6. Cutting v0.1.0

After sections 1 to 3: run the `release` skill (`.agents/skills/release/SKILL.md`).
It prepares `release-0.1.0`, waits for CI on the exact head, tags `v0.1.0`, and
`release.yml` builds, attests, publishes and smoke-tests. Verify with the commands
of `.github/SECURITY.md` ("Verifying a release").
