# Pixel in the Pi harness

Run `pixel install` to install Pi's command-only `/pixel-impact` extension, then
`pixel install --repo .` in each repository for task lifecycle gates. Trust the
project in Pi. The global extension registers no model-callable Pixel tool and
does no startup retrieval. Invoke `/pixel-impact <symbol>` only when caller or
impact evidence is useful; it runs `pixel impact <symbol> --no-refresh --depth
2 --json --metrics off` once against the existing graph. Missing, stale,
unsupported, or slow results fall back to native search without repairing or
refreshing the index.

The project extension at `.pi/extensions/pixel-guard.ts` retains task
authorization, telemetry, and stop gates. Its older model-callable retrieval
tool, per-prompt context, native retrieval policy, and post-edit impact advice
are disabled by default. Set `PIXEL_PI_RETRIEVAL=1` only to opt into that
legacy retrieval adapter. `pixel doctor .` checks the project extension.

The opt-in adapter exposes these additional operations:

| User outcome | Stable action | Pixel CLI operation |
| --- | --- | --- |
| Find likely files for a task | `scope_task` | `scope-task` |
| See repository areas | `list_areas` | `list-areas` |
| Search text | `search_content` | `search-content` |
| Locate code by phrase | `find_code` | `find-code` |
| Inspect symbol effects | `impact` | `impact` |
| Read focused symbol context | `pack_context` | `pack-context` |
| Inspect changed symbols | `what_changed` | `what-changed` |
| Review the working tree | `review_changes` | `review-changes` |
| Fetch remote refs | `fetch` | `fetch` |
| Commit named files | `commit` | `commit` |
| Commit and push named files | `commit_and_push` | `commit-and-push` |

The action names remain stable if Pixel's CLI spelling changes. The extension
reports an explicit error if the installed executable lacks an operation.
Responses include bounded evidence, truncation, index and graph state, and a
next action when results are capped or Pixel is unavailable. The extension
checks `pixel status` before invoking an action. If the index is missing, run
`pixel build-index --history .`. If the graph is missing, run
`pixel rebuild-graph .`. Then retry. Native tools remain available under the
default advisory policy. History facts can lag; their `fresh` field is returned.

## Optional legacy native tool policy

Set `PIXEL_PI_RETRIEVAL=1` before starting Pi to enable this compatibility
adapter. Then choose its policy with `pixel config policy`, or for one
environment with `PIXEL_POLICY`:

| Value | Behaviour |
| --- | --- |
| `advisory` (default) | Suggest Pixel retrieval while preserving native tool inputs and execution. |
| `enforce` | Redirect supported simple retrieval commands and apply the scoped read/edit gates described below. |
| `off` | Skip classification, policy logging and read/edit gates. |

`pixel config policy enforce` writes the repository layer
(`<repo>/.pixel/config.yaml`); `--global` writes the machine-wide
`~/.pixel/config.yaml`. The repository file wins over the global one,
`PIXEL_POLICY` overrides both for one environment, and an absent or
unrecognised value selects `advisory`. The extension reads the effective
setting once per project (`pixel config policy --json`) and keeps the
advisory default when Pixel cannot answer. The legacy `PIXEL_TARGETS_GUARD=0`,
`false`, or `off` also disables the policy. These settings do not disable the
structured Pixel tool's write authorization.

In `enforce` mode, supported simple `ls`, `rg`, `grep`, and repository Git
commands receive a redirect to the structured Pixel tool. The extension does
not silently replace a native command with a different operation. Pi's in-repository `read` tool uses
Pixel-resolved paths with a limit of at most 200 lines; outside paths are
exempt. The mode can refuse supported repository retrieval with a redirect.
Shell compositions, redirections, interpreters and unknown capabilities
remain intact when the extension cannot classify them reliably. For example,
`cargo test | tail -20`, `cargo test | rg error`, and
`pixel repo-state --json | jq .branch` keep their original shell semantics.
No leaf is removed or executed separately.

The extension checks Pi tool calls, including read, bash, and named discovery
tools. It cannot intercept Pi's own project context loading before an agent
turn, file access performed *inside* an allowed build or test process, or a
tool process that bypasses Pi's `tool_call` event. The policy is a retrieval
workflow preference, not a repository sandbox. Pi's permissions remain
authoritative. The extension's `classify()` is deterministic local code;
it does not invoke the model-based `pixel classify` command.

## Task lifecycle and optional legacy retrieval

Task authorization, telemetry, and stop gates remain active independently of
retrieval settings. The explicit impact command is user-invoked and adds only
its bounded query result to the current session.

When `PIXEL_PI_RETRIEVAL=1`, the legacy project adapter also registers its
structured tool, automatic task context, retrieval policy checks, and post-edit
`what-changed` advice. This compatibility mode can add startup calls and
context; it is not the default experience.

`commit` and `commit_and_push` require explicit user intent in the current
user message, plus named files, a message, and an idempotency request ID.
`commit_and_push` requires intent to push as well. A fetch never grants it.
The extension cannot cryptographically authenticate natural-language intent;
it applies this check at tool execution and reports denials.

Policy decisions are logged in `.pixel/pi-policy.jsonl` with decision kind,
tool, reason, and health or truncation metadata. The log does
not record commands, query strings, file contents, or commit messages. Count
policy entries to inspect which calls received advice or were blocked. This is an operational
trace, not an audit of reads by allowed child processes.
