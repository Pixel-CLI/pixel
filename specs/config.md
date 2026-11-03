# Configuration

Status: normative for `pixel config` and for how the other specs read
settings. The commented template a fresh file starts from is
`crates/pixel/src/config.yaml`; the install-time use of the file is ARCHITECTURE.md,
"Agent integration".

## Layers

Two YAML files, nearest first:

1. the repository's `<root>/.pixel/config.yaml`;
2. the global `~/.pixel/config.yaml` (created 0600 when it holds keys).

An environment variable beats both, and a missing key falls to the default:
**environment, then repository, then global, then default.** Some keys are
global only: `classify` and `remote_keys` (a repository copy has no effect), and
`web_search`. A file that does not parse, or a known key of the wrong type, is
reported with the file's path and the rule, never silently ignored.

## `pixel config`

| Form | Does |
|---|---|
| `pixel config` | prints the paths of both layers and each effective setting with the layer that set it: `metrics`, `daemon_auto_start`, `brief`, `brief_parts`, `classify.enabled`, `classify.engine`, the web-search provider. Creates no file; prints no secret. |
| `pixel config <key>` | prints `<key>: <value> — <source>` where the source is an environment variable, a file path or `default` |
| `pixel config <key> <value> [--global] [--path P]` | validates, then writes the repository layer of the repository that contains `P` (default `.`), or the global one with `--global`; then prints `<key>: <value> — wrote <file>` |

The flat settings `pixel config <key> <value>` writes are exactly four:

| Key | Value | Environment | Default |
|---|---|---|---|
| `daemon_auto_start` | `on`/`true` or `off`/`false` (stored as a boolean) | `PIXEL_DAEMON_AUTO_START` | on |
| `brief` | `on`/`off` | `PIXEL_BRIEF` | on |
| `brief_parts` | a whole number from 1 to 8 | none shown by `pixel config`; the pack builder also reads `PIXEL_BRIEF_PARTS` (`brief.md`) | 3 |
| `edit_hooks` | `on`/`off` | `PIXEL_EDIT_HOOKS` (the hook alone; `pixel setup` registers the edit hooks from the global layer only, `hooks.md`) | off |

An environment value of `0`, `false` or `off` is off, anything else on.

Rules of a write:

- **Validate first, write nothing on error.** An invalid value exits 1 with a
  message that names the key and the rule (`brief takes on or off`,
  `brief_parts must be a whole number from 1 to 8`) and never the value, so a
  mistyped credential cannot travel in an error.
- **A key that is not a setting is refused**, with the list of settings and a
  pointer to `pixel config remote-key`. That includes every credential key such
  as `remote_keys.openai`. `logged_args` records the value of a known setting
  and masks the value of anything else.
- **Say when it does not take effect.** If a higher layer or the environment wins,
  the write prints `note: <source> sets <key>: <value> here and wins`. After a
  `brief_parts` write it prints `note: run `pixel setup` so the registered hooks
  carry the new count`: the count is the number of hook entries `pixel setup`
  registers (`hooks.md`), and `pixel doctor` flags a mismatch.
- **Never write through a link.** A `.pixel` or `config.yaml` that is a symbolic
  link, or a `.pixel` that is not a directory, is refused. The write replaces
  the file through a private temporary file (0600) renamed over it, and keeps
  the other keys and the comments of the template.
- With no repository root and no `--global`, a write exits 1 with `no
  repository root here — pass --global or run inside a repo`.

## Subcommands

| Subcommand | Effect |
|---|---|
| `pixel config setup` | guided global setup; needs a terminal and refuses piped input without writing |
| `pixel config classify on\|off` | the global `classify.enabled` switch (`classify.md`); also repairs a non-mapping `classify` section |
| `pixel config classify-engine <engine>` | stored `classify.engine` (`auto`, `local`, `remote`, or a hosted preset) |
| `pixel config remote-preset <preset> [--model M] [--base U]` | stored remote preset; a `--model` or `--base` left out clears the previous provider's stale choice |
| `pixel config remote-key <preset> [<key>\|-] [--clear]` | stores, reports (`set`/`unset`) or clears a key in the global file; `-` reads the key from stdin; the key is never printed back |
| `pixel config metrics [on\|off] [--global]` | reports the effective live-metrics setting and its layer, or persists it; a global write reports a repository layer that contradicts it |

## Keys that have no `pixel config <key> <value>` form

Edit the YAML by hand: `brief_window_ms` (250 to 4,500, `brief.md`),
`task.enforcement` (`enforce`, `advisory` or `off`, default advisory;
`hooks.md`), `task.checks`/`criteria`/`inputs`/`outputs`/`conservative_checks`/
`toolchain` (the task contract), `web_search`, and the per-engine fields of
`classify` (`remote_model`, `remote_base`).

## Tests that prove it

`config_cli` (the overview creates no file and prints no secret; a known setting
is written to the repository layer and read back; a global write leaves the
repository alone and names a layer that wins; an invalid value is refused and
writes nothing; an unknown key lists the settings and never echoes or logs its
value; the action log keeps the value of a setting it can show; `brief_parts`),
`confined_repo_config_should_refuse_a_file_where_dot_pixel_belongs`, and
`specs_drift` (the three settings above are the ones the binary lists).
