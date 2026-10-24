# Agent skills

Status: normative for `pixel-setup` skill deployment.

The skill is a **fallback**, never the mechanism (`thesis.md`): measured models
do not call optional tools. It exists for what the hooks did not inject and for
hosts that have no hook (Cursor, Copilot CLI, Amp).

## The `pixel` skill

`skills/pixel/SKILL.md` = front matter (`name: pixel`, a one-paragraph
`description`, `license: MIT`) + `assets/pixel-skill.md`, **at most 2 KB**
(Pixel evidence: 16.3 KB → 4.6 KB of doctrine helped; a 2.8 KB payload passed
the gate). Content, in order:

1. `[PIXEL:BRIEF]`, `[PIXEL:EDIT]`, `[PIXEL:STALE-CALLERS]` and a Stop block are cited
   facts to act on; open a cited region only if it contradicts you.
2. A query table, only for what the hooks did not inject: `find-symbol`,
   `who-calls`, `impact`, `find-code`, `list-signatures`, `pack-context`,
   `what-changed`, `review`, `rename --dry-run`.
3. Lower bounds: `0 callers`, `capped`, `closed_world=false` mean "none found",
   not "none exist".
4. Native tools are right for regex and literal searches, path questions and
   single-file edits.
5. Where the host has no relay, copy the `🟩 pixel` line from the tool result once.

`docs_drift` ties the skill's command names to `pixel --help`.

## Where it goes

`<root>/skills/pixel/SKILL.md` for each configured harness root: `~/.claude`,
`~/.codex`, `~/.cursor`, `~/.devin`, `~/.gemini`, `~/.config/opencode`, and
`~/.agents` (read by Copilot CLI and Amp). Pi's copy comes from its package
step; Antigravity's from its two plugin dirs. A guided run writes only the
selected agents' roots. A differing existing copy is backed up first; `--remove`
deletes a copy only while it still equals what was written.

## `pixel-classify`

From Pixel's `pixel-classify-skill.md`, offered only when a classify engine is
accepted, into the same roots (and the pi classify tools, `hosts.md`).
`--remove` renames a user-edited copy outside `skills/` so the host stops
loading it.

## Plugin skills

A plugin manifest's `skill` key names one text file inside its directory (a
regular file, no link, ≤65,536 bytes, UTF-8, validated before anything is
written). `pixel install` delivers it to `.claude` and every configured root;
`pixel uninstall` removes a copy only while it still matches. A plugin named like
a core skill is refused. (`deploy_plugin_skill`/`remove_plugin_skill`, lifted.)

## Ownership

Setup manages the `pixel` and `pixel-classify` skills through its own receipt
and backups. It does not run a competing-Pixel migration sweep. A differing
existing skill follows the same backup and restore rules as any existing
configuration (`setup-migration.md`).

## Tests that prove it

Size cap on the rendered skill; every `pixel <command>` in it exists
(`docs_drift`); deploy/backup/remove round trips; a user-edited copy survives
`--remove` and is reported; doctor flags a stale or edited copy.
