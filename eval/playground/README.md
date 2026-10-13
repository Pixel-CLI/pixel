# Brief playground

A local web page to look at what `pixel brief` returns for a prompt, and why
(issue [#901](https://github.com/Pixel-CLI/pixel/issues/901), phase 1). One
Bun server, one HTML page, no framework, no build step.

## Run

```bash
cd eval/playground
bun install        # only @types/bun
bun run dev        # http://127.0.0.1:4901  (PORT=... to change; check it is free first)
bun test           # API self-test against a fake pixel binary
```

`bun run start` is the same without hot reload.

## What it shows

- **Left:** the prompts of `eval/brief-gate/prompts.jsonl` plus the ones you add
  (`local-prompts.json`, gitignored). Filters: search, kind, on/off topic, language.
- **Right:** the brief for the selected prompt. The `[PIXEL:BRIEF]` header,
  `searched:` receipt and `confidence:` line are highlighted; `read:`, `also:` and
  `files:` entries are clickable `path:line` chips that open the cited range from
  the fixture. A "why" box shows gate, tier, score, signal, kind and the
  gate features (`struct_ratio`, `struct_per_mille`, `best_structural_weight`,
  `total_weight`, the rest under a fold). Byte size is drawn against the 2048 B
  cap and latency is the wall time of the call.
- **Compare two binaries:** tick the box in the header for a second column.
- **Run in Codex:** a disabled placeholder (with Pixel / without Pixel / abort);
  it lands with #900.

## Fixture and binaries

`POST /api/brief {prompt, bin}` runs `<bin> brief --json --metrics off "<prompt>" .`
with cwd = the fixture, timeout 5 s. A binary without `--json` is retried
without it and the raw text is shown, with no decision record.

- **Fixture:** `PLAYGROUND_REPO`, default `/private/tmp/claude-501/883-ab/zero/pixel`
  when it exists, else this checkout. It must be indexed. To build one:
  `bun run setup [dest] [pixel-binary]` clones the repository at `85bede7d`
  (`git clone --no-local`) and runs `prepare-repo`.
- **Binaries:** `bins.json` (gitignored), a JSON array:
  ```json
  [{ "name": "installed", "path": "/Users/me/.cargo/bin/pixel" },
   { "name": "PR 891", "path": "/private/tmp/claude-501/883-ab/zero-pixel" }]
  ```
  Without it, the defaults are the installed `pixel` and the 883-ab builds that
  exist. Only paths in this list can be run.

## API

| Route | |
| --- | --- |
| `GET /api/prompts` | corpus + local prompts |
| `POST /api/prompts {text, lang?, kind?, on_topic?}` | add a local prompt |
| `GET /api/bins` | binaries, whether each exists and supports `--json` |
| `POST /api/brief {prompt, bin}` | brief text, parsed ranges, decision record, bytes, latency |
| `GET /api/file?path=&start=&end=` | a range of the fixture (paths outside it are refused) |

The server listens on 127.0.0.1 only.
