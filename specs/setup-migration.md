# Setup, doctor and removal

Status: normative for `pixel-setup` (install, uninstall, receipt, doctor).
Setup writes the product's hook registrations and optional host assets. It
preserves every unowned hook and setting; it has no competing-product migration
sweep. Existing Pixel hook commands in the legacy `run-hook` shape remain
foreign, while this installation replaces its own current `pixel hook` entries.

## `pixel setup`

- Guided on an interactive terminal: detect configured hosts, ask which
  integrations to install, show the exact writes, then wait for one confirmation.
- Unattended with `--yes`, `--json` or CI: install for every configured host;
  `--agent` limits the selection and `--print` previews writes.
- The repository-scoped install API writes only that repository's
  `.pixel/config.yaml`.
- `--remove` restores or removes only files the setup receipt proves this
  installation owns.

## Writes and ownership

Setup registers the configured hosts' lifecycle hooks, optional host plugins,
the Pixel skill, and optional classification assets. JSON updates preserve
foreign entries. Re-running replaces only entries matching this executable's
Pixel hook identity, so hooks are not duplicated. Unknown and older hook
commands remain untouched.

The receipt stores original bytes and modes for changed files, fingerprints for
installed files, and directories created by setup. Removal validates receipt
paths against setup's output paths, restores unchanged files, removes unchanged
created files, prunes only created empty directories, and keeps later edits.
Writes use adjacent backups and atomic replacement. Invalid settings are left
untouched and reported. All install and removal tests use fixture homes.

## Doctor

`pixel doctor` checks the executable, configured host hooks, host assets,
repository config, daemon and index freshness. Checks report green, yellow or
red, with an automatic repair command where setup can safely repair the state.
Doctor does not classify other products' hooks as conflicts or modify their
installations.

## Contract tests

Tests prove setup followed by setup is idempotent, foreign hooks and settings
survive, malformed settings are not rewritten, and setup followed by removal
restores original bytes while retaining files edited after setup. Guided and
unattended setup use the same selected writes.
