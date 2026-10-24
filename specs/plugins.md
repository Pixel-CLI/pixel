# Plugins

Status: normative for `pixel-plugin`. Lifted from `pixel-plugin` (API 1) minus the
`MOVED_COMMANDS` table: Pixel's core knows no moved commands. The git commands,
flow, recall, error listing and utilities stay in their own plugin repositories
and stay outside the core.

## Manifest

`pixel-plugin.toml`: `name`, `version`, `api = 1`, `description`, `run`,
`capabilities` (`command`, `brief-probe`), `network`, optional `commands` (one
binary serving several `pixel <command>` words, run as `<run> <command> <args…>`),
optional `skill`. Two plugins of a tier answering to one command are an error
naming both.

## Lookup, trust, install

Resolution order: trusted repo plugin (`<repo>/.pixel/plugins/<name>/`), user
plugin (`~/.pixel/plugins/<name>/`), `pixel-<name>` on PATH (an implicit
manifest-less plugin). Trust store `~/.pixel/trusted.toml` (name + SHA-256 over
every file of the directory: any change revokes). `run` is canonicalised and
must stay inside the directory. Network opt-in and install sources live in
`~/.pixel/plugins.toml`. `pixel install` takes a local directory, a shallow
`git clone` (through the single git runner) of an `https://`, `ssh://`,
`file://` or `git@` URL, or `github:<owner>/<repo>[@<ref>]` with the program
fetched as a release asset checked against `<asset>.sha256` (a release without
one is refused unless `--allow-unverified`); `npm:` is refused.
`pixel uninstall`, `pixel plugin list|enable|disable|trust`. Directory copies pin the
source root and open children relative to directory descriptors (rustix); links
are refused and device/inode compared with the listing; the digest encoding is
unchanged so trust records survive.

`PIXEL_GITHUB_BASE_URL` redirects `github:` sources and their release downloads
(a mirror, GitHub Enterprise, or a `file://` tree laid out the same way); it is
the CLI's way of filling `Host::github_base`.

An unknown subcommand runs the plugin of that name with `PIXEL_API`,
`PIXEL_REPO_ROOT`, `PIXEL_GRAPH_DB`, `PIXEL_BIN` in its environment; home, repo root,
PATH and the GitHub base URL are fields of a `Host` struct, never read from the
process inside library code.

## `brief-probe`

The manifest keys parse and validate (`pixel plugin enable|disable` switch a
probe); the runner is `pixel-brief/src/probes.rs` (`brief.md`, "Third-party
probes").

A plugin with `capabilities = ["brief-probe"]` and a `[brief-probe]` table
(`owner`, `when_kinds`, `skip_kinds`, `when_prompt` regex, `when_gate_closed`,
`budget_ms` ≤2,500, `mode` lead or append) adds blocks to the brief (`brief.md`).
Pixel bundles none. Probes run in parallel with a cleared environment (`PATH`,
`HOME`, `LANG`, `GIT_CONFIG_NOSYSTEM`, `PIXEL_BIN`), in their own process group,
inside `min(budget, deadline)`; a repository's probes run only at the digest
`pixel plugin trust` recorded. `PIXEL_BRIEF_PROBES=0|off|false` runs none.

## Tests that prove it

Lifted `pixel-plugin` suites; trust revoked on any file change; link refused at
every path component; two plugins of one command named in the error; probe
timeout kills the process group; a manifest `skill` that fails validation
installs nothing anywhere.
