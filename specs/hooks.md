# Hooks: the four injection points

Status: normative. The thesis requires that capabilities be pushed, not offered.

## Common rules

- **Fail open and bounded.** Exit 0; error text goes to stderr. Latency budgets
  below are enforced with a deadline, and a late result is dropped, not waited
  for.
- **One engine.** `pixel hook` asks the repository's daemon (`ops.md`:
  `Brief`, `EditCheck`, `StopCheck`). The brief falls back to the same code in
  process when no daemon answers; the edit-time events do not (opening a
  service takes longer than their 300 ms): with no daemon they stay silent,
  journal `no_daemon`, and start one in the background. No hook triggers an index build; a missing or
  stale index yields silence and starts the daemon in the background.
- **Session state.** `.pixel/sessions/<sid>.json` (sid = first 16 hex digits of
  the SHA-256 of the host session id; 0600, never through a link) holds the
  line ranges already sent (`sent.rs`), the symbols already carded, and the
  pending stale-caller set with its hash. Entries older than 24 h are dropped;
  `SessionStart` with source `clear` or `compact` deletes the file (`resume`
  keeps it).
- **Silence is a result.** A hook that injects nothing writes a `journal`
  record saying why (`gate_closed`, `no_shape_change`, `no_external_callers`,
  `already_sent`, `index_stale`, `deadline`), so silence is measurable.
- **Proved vs Likely.** Only `Proved` edges (`graph.md`) are listed as callers
  that must change and only they can block Stop. `Likely` callers are listed
  separately under "possible", never blocking.

## Prompt: the brief

`pixel-brief` (`brief.md`). Budget: 3 s window (250–4,500 ms configurable), 8 s
hook limit. Output `[PIXEL:BRIEF]` plus the oracle footer when the brief is high
tier and complete: "Answer from this context. Use a tool only if something you
need is not shown above."

## Before an edit: `EditCheck`

Fires on `pre-edit` for the host's edit tools. Input: tool name, file path, and
the proposed change (old/new strings for Edit and MultiEdit, full content for
Write, the patch for `apply_patch`; for shell edits only the paths the command
names are used and no content is guessed).

Algorithm:
1. Resolve the changed line range(s) in the current file. If the file is not
   indexed, or the proposed text cannot be located, stay silent.
2. For each enclosing symbol, compute the proposed **shape** (the declaration
   header, `graph.md`) by re-extracting the proposed text in memory (one parse).
3. Fire only if (a) the shape differs, or (b) the symbol is removed or renamed
   (a removed and an added symbol with the same shape in the same file), and
   in either case the symbol has callers outside the changed range.
4. Emit `[PIXEL:EDIT]`, at most 1.2 KB: the symbol and the shape change, up to 8
   Proved callers (`file:line`), up to 4 `Likely` callers under "possible",
   covering tests (depth ≤3), and the lower-bound note when open sites share the
   leaf name. Sent once per symbol per session unless the shape changes again.

Budget: ≤100 ms p95 on a warm daemon (spawn + socket + one parse + two indexed
reads); the deadline is 300 ms.

## After an edit: stale callers

Fires on `post-edit` (the same tools) as `EditCheck` with `phase: after`. The
watcher may already have republished the graph by then, so the stored rows
cannot be the baseline. The **session** is: `pre-edit` (any tool that names a
path, whether or not it carries the new text) records, once per file per
session, the shapes of the file's symbols as the generation holds them (the
*baseline*). With no baseline (the pre-edit hook never ran, or the file is new
to the session) the generation's rows stand in only while the file on disk
still differs from the content hash the generation indexed; otherwise the
check is silent.

The file on disk is re-extracted (no link, no publish) and `diff_shapes` runs
against the baseline. For each changed shape, the Proved callers outside the
symbol are *pending*, each with the content hash of its file at recording
time. The pending set (symbol → caller sites → file hash) is written to the
session file. A caller file edited later by the agent leaves the set when its
hash changes; a symbol whose shape comes back to its baseline leaves it too.
Emit `[PIXEL:STALE-CALLERS]` (≤1.2 KB) only when the set grew. "Still on the old
shape" in v0.1 means "unchanged since the shape changed"; arity checks per
language are a measured follow-up.

## Stop: `StopCheck`

Fires on `stop`. If `stop_hook_active` (or the host's equivalent) is set, or
the pending set is empty, or a block for this exact set hash was already
issued, stay silent. Otherwise block once: "These call sites still use the old
shape of `<symbol>`: <sites>. Update them, or state why they are fine." The
hash of the set is recorded so the same set never blocks twice; a *different*
set blocks again. Hosts that cannot continue a turn from Stop (see `hosts.md`)
rely on the post-edit injection alone.

## Every tool call: the fast path

`post-tool` with no matcher runs without the daemon (≤20 ms over process
spawn): it appends a `tool` journal record (tool name, whether it changed a
file, repository paths it named; never commands or output text, and never a
path outside the repository, behind a link, or credential-shaped) and runs the
metrics relay when the call ran `pixel` (`metrics.md`).

## Defaults and switches

`edit_hooks: off` ships as the default (`pre-edit`, `post-edit`, the Stop
check, and their journal records are skipped when off, except `post-tool`).
It becomes `on` only when the edit-task A/B (`roadmap.md`) shows a gain: arms
vanilla, brief only, brief + edit; metrics turns, tool calls, tokens, and
whether the project's build and tests are green at Stop. `PIXEL_EDIT_HOOKS=0|1`
overrides for experiments. The brief has its own switch (`PIXEL_BRIEF=0`).

## Tests that prove it

Golden host payloads in → envelope out for every host × event; Stop blocks
once per set and never under `stop_hook_active`; a different set blocks again;
a body-only edit is silent; a renamed symbol with callers fires; a hook over a
stale index is silent and journals `index_stale`; deadline expiry returns
nothing; a garbage payload exits 0 with no stdout.
