# Codex retrieval arena

Seven containerized arms compare retrieval tools against native Codex: `raw`,
`semble`, `graft`, `stacklit`, `gitnexus`, `gortex`, and `pixel`. Each arm uses
its own writable repository snapshot; each repetition gets a fresh snapshot.
Authentication is mounted at runtime, never baked into an image.

Build the shared base before the first run:

```bash
rtk docker build -f eval/arena/Dockerfile.base -t pixel-arena-base:latest eval/arena
```

## Tiny paired loops

Start with one scenario, one repetition and two arms. Use a new output directory
for every run so an earlier failure or a different candidate cannot enter its
comparison.

```bash
REPO_SNAPSHOT=/path/to/foreign/repository \
PIXEL_SRC=local \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
rtk bash eval/arena.sh --arms "raw pixel" --tasks "g1-locale-routing" \
  --reps 1 --assert-context-parity \
  --results-dir eval/arena-results/g1-candidate-01
```

`REPO_SNAPSHOT` supplies the repository being investigated. Local Pixel source
comes from the repository containing this script, or `PIXEL_SRC_DIR` when set.
Use the same repository revision, model, effort and scenario for both arms.
The `g*` scenarios target the foreign example application; the `s*`
scenarios target Pixel itself. A scenario's expected paths must exist in the
chosen repository.

To reuse a particular candidate, set `PIXEL_IMAGE_SOURCE=existing` and
`PIXEL_ARENA_IMAGE=sha256:<image-id>`. The runner resolves every selected image
once, checks the actual container image before starting it, and saves that
receipt beside the results. It also refuses comparisons across different Codex
versions. Local builds cache Cargo's registry, Git dependencies, and build
artifacts; the first build still needs to populate those caches. Git builds use
the resolved commit as Cargo's actual `--rev`.
The runner bind-mounts its current entrypoint, context manifest, hook auditor,
and skill staging helper
into each container, including when reusing a pinned image; harness-only edits
do not require an image rebuild. `harness-source.json` records SHA-256 values
and container paths for the runner files used in that run.

With `--watch`, Herdr panes run the normal interactive `codex` TUI, submit the
same first scenario prompt, and use Codex's
`--dangerously-bypass-approvals-and-sandbox` flag for YOLO mode. Do not use
`codex exec` or `--full-auto` for the visible pane run. When launched outside a
Herdr pane, the runner discovers the focused pane; set `HERDR_PANE_ID` when
that is ambiguous. The pane waits for the arm's setup-ready marker, so Pixel's
hooks are installed before the visible Codex session starts.
The repeatable stale-pane/layout/prompt-submit drill is also recorded in the
`codex/700-native-default` history and its follow-up `wip: arena watch
prompt-submit edits` stash.

Graph experiments use `--prepare-pixel-graph`; the saved setup receipt separates
index preparation from model time. Retrieval routing stays native by default,
including when a graph is prepared. A fresh Codex home may leave installed
hooks untrusted: static context parity alone cannot establish hook delivery.
`--assert-context-parity` additionally expects no Pixel calls and is intended
for generic-question abstention controls.

Every Pixel arm now runs a deterministic preflight after setup. It sends a
fixed Codex `UserPromptSubmit` payload through `pixel run-hook prompt-submit`
and requires a non-empty valid response. The result is saved as
`pixel-brief-hook-<rep>.json`; `response_valid` is the pass/fail check and the
receipt stores only context type, size, and hash, not the prompt. A valid
prompt hook can return either `[PIXEL:BRIEF]` or `[PIXEL:EXECUTION_ROUTE]`;
those are distinct context shapes and neither is inferred from a scenario name.
The runner also prints the full returned context between explicit begin/end
markers during preflight and after a live watched submission.
Native-default installs intentionally do not register this retrieval hook.
Use `--review-pixel-hooks` with a reviewed hook image to require the live
Codex `UserPromptSubmit` wrapper receipt (`pixel-hook-<rep>.jsonl`) with a
valid, forwarded, non-empty context.
The default setup runs `pixel prepare-repo --no-daemon` so the brief gate sees
the repository's Pixel shard before Codex starts.

### Explicit reviewed-hook experiment

The default run keeps Pixel retrieval routing native and does not bypass Codex
hook trust. For a disposable raw/Pixel pair that intentionally exercises
Pixel's Codex prompt hook, add `--review-pixel-hooks`. This flag only audits and
records delivery; it does not enable experimental caller facts. Both containers
must finish the audit before either model starts: raw must have no hooks; Pixel
must have exactly the 11 known Pixel Codex commands under
`/root/.codex/hooks.json`; project/global foreign hooks and plugin hook
declarations stop the pair. Only after both audits pass does the visible Pixel
Codex session answer the normal hook-review dialog with `Trust all and
continue`, confirms that the dialog disappeared, and only then submits the
scenario prompt. The runner never passes Codex's hook-trust bypass flag. The
Pixel prompt-hook wrapper records its validated `UserPromptSubmit`
response, emitted context status and hook stderr in
`pixel-hook-<rep>.jsonl`; task stderr is retained as
`<arm>-<task>-<rep>.stderr`. Neither receipt stores prompt input or auth. A
receipt proves only that context text was returned and forwarded by the hook;
it does not prove the model used it. Reviewed-hook mode is limited to one task
per run so the receipt corresponds to the selected scenario; the harness marks
the Pixel result failed if that task produces no valid hook response.

### Skill-only pilot

Use a packaged Codex skill whose `agents/openai.yaml` explicitly sets
`allow_implicit_invocation: false`. The disposable Pixel snapshot gets a copy
with that policy enabled; the packaged source is never edited. Raw and Pixel
use the same pinned Pixel image and graph preparation, but neither runs
`pixel install` or installs retrieval hooks. Only Pixel receives the candidate
skill. The runner captures discovered skills, skill and policy hashes, hook
configuration fingerprints, static instructions, and setup time, then checks
that the candidate skill is the only context difference and that no Pixel
instruction/hook text remains in either arm.

The candidate must be supplied by the packaging lane after its install/image
is ready. The arena does not build or install a skill package itself:

```bash
REPO_SNAPSHOT=/path/to/foreign/repository \
PIXEL_IMAGE_SOURCE=existing PIXEL_ARENA_IMAGE=sha256:<pinned-image-id> \
CODEX_MODEL=gpt-5.6-terra CODEX_EFFORT=medium \
rtk bash eval/arena.sh --arms "raw pixel" --tasks "g5-transfer-status-impact" \
  --reps 1 --skill-candidate-dir /path/to/packaged/pixel-impact \
  --results-dir eval/arena-results/g5-skill-pilot-01
```

This mode is diagnostic, not a general no-regression guarantee. Skill
discovery is recorded separately from explicit skill-file reads and Pixel CLI
calls. Absence of a file-read event does not prove the model did not load the
skill, and a CLI call does not by itself establish usefulness. Review semantic
correctness and task-specific edit/consumer completeness separately from the
required-pattern score. Use a fresh output directory for each candidate.

### Claude skill-only diagnostic

`eval/claude_skill_pair.py` runs a one-repetition raw/skill pair after a
no-model preflight. It uses the same foreign-repository snapshot and g5
scenario, with `sonnet` / `medium` by default. It resolves `claude` and `pixel`
from `PATH` unless explicit binary paths are supplied; a missing executable
fails with a clear message. On macOS it reads raw Keychain JSON for the current
account and uses only its `claudeAiOauth` object. Without a configured scope it
selects Claude's unscoped login; `--auth-config-dir`, or the existing
`CLAUDE_SECURESTORAGE_CONFIG_DIR` / `CLAUDE_CONFIG_DIR` scope, selects that
profile's entry without falling back to another profile. This default OAuth
mode never imports user settings, skills, hooks, plugins, or MCP configuration. An optional
`--credentials-file` takes precedence over the
config directory's `.credentials.json` and must have mode `0600`. Only that
OAuth object is copied into each private, temporary arm config; credentials are
never written to results. Exact access/refresh token values and their JSON-
escaped forms are redacted from Claude stdout/stderr before parsing or saving;
this is exact-value filtering, not general secret detection. The preflight
refuses an expired refresh token.

```bash
PAIR_ARGS=(--repo /path/to/architech-t --scenario eval/scenarios/g5-transfer-status-impact.json \
  --skill claude-skills/pixel-impact/SKILL.md --results-dir eval/arena-results/claude-g5-r1)
python3 eval/claude_skill_pair.py preflight "${PAIR_ARGS[@]}"
python3 eval/claude_skill_pair.py run "${PAIR_ARGS[@]}"
```

Run the second command only after reviewing a successful preflight. OAuth mode
requires a usable existing OAuth login. A failed or missing arm remains a failed
pair, with unavailable token fields recorded as unknown.

For an existing Claude gateway connection, explicitly add
`--auth-mode configured-gateway --gateway-settings /path/to/settings.json` to
both commands. This mode reads only the supported authentication, endpoint,
header and model-selection environment fields; other settings, hooks, skills,
plugins and MCP configuration remain excluded. Both arms receive the same
in-memory connection settings in otherwise isolated environments. Preflight
records the settings file's path and file identity; changing the file requires
a fresh preflight. Private values and their hashes are excluded from receipts. Record this as a separate
gateway experiment, without pooling it with another provider or model.

`claude auth status` alone is insufficient: host settings can supply a different
endpoint, token, or model while an isolated OAuth comparison has no usable
credential. Preflight checks configuration without making a model request;
only the actual pair establishes whether the connection works. Claude's reported
model identity must agree between arms, but does not attest which model an
external gateway runs internally.

The snapshot check permits writes to Pixel's regular accounting files,
`.pixel/actions.jsonl` and `.pixel/calls.json`. It still detects source edits,
graph changes, unexpected sidecars and changes to those files' types.
Before each arm, the prepared source snapshot, Pixel binary, graph and recorded
query output must still match preflight. Validation time is recorded separately
from model runtime. Both arms receive the same narrow
`Bash(pixel impact * --no-refresh *)` permission allowance.

The old `--codex-caller-facts` route is retired and rejected. Historical runs
remain in their original result directories and must be interpreted with
their recorded source/image identities.

The hook allowlist intentionally matches the current Pixel Codex installer.
If its command set or event names change, update the allowlist and tests before
using this reviewed-hook mode.

Inspect the transcripts as well as the rank table. A required-pattern score
measures answer coverage, not semantic correctness. Token totals include
reported input and generated tokens; cached input is part of input and must not
be counted twice. Unknown usage is unknown, not zero. Failure counts belong
beside completed-pair measurements; a transcript containing an answer does not
by itself explain a nonzero container exit.

Keep the first loop diagnostic. A single pair can expose a forced extra lookup
or an incorrect answer, but cannot establish a general speedup. Confirm useful
changes on another generic question and on the structural question meant to
benefit. Preserve the source identity, image identity, complete logs and exit
statuses with each result. Change one input between candidate comparisons.
