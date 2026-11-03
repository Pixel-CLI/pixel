# Classify

Status: normative for `pixel classify`, the global switch `classify.enabled`
and the brief's use of it. The brief's side (when a `none` refuses a prompt) is
stated in `brief.md`; this file states the command and the data it moves.

`pixel classify` is a decision over a bounded label set: state text, shared
`--context`, labels with an optional `--criterion` each; the answer is one
probability per label, renormalised to sum 1, and the predicted label. It is
**opt-in, non-deterministic and, with a remote engine, network-bound.** The
default path of every other command makes no model or network call.

## The switch

- `classify.enabled` lives only in the global `~/.pixel/config.yaml` and
  defaults to off. `pixel config classify on|off` writes it and keeps the
  engine choice and credentials. `classify.enabled` set in a repository
  `config.yaml` has no effect (`config.md`).
- Checked before any input is read or any engine is opened: with the switch off
  or unreadable (`classify.enabled must be true or false`), `pixel classify`
  prints `pixel: classify is disabled; enable it with `pixel config classify on`
  or `pixel config setup`` to stderr and exits 1, **including** with an explicit
  `--engine` and with `--jsonl`.
- The brief reads the same switch (`brief.md`). `pixel config setup` (a
  terminal) asks whether the classifier is allowed and runs the engine proposal;
  a failed first-time installation leaves it off.

## Engines and presets

| Engine | Behaviour |
|---|---|
| `--engine remote` | an OpenAI-compatible chat completion; probabilities are verbalised by the model, not calibrated |
| `--engine ollaya` | a local Ollaya server's typed-choice readout (`--ollaya-url`, default `http://127.0.0.1:11435`); without `--label` it answers its default question battery |
| no flag | the stored `classify.engine` (`pixel config classify-engine <engine>`): `auto` (default: probe a local Ollaya, else remote), `local` (Ollaya), `remote`, `jev`, `clef-ollama`, `clef-cloudflare` (the last three are the remote engine on that preset) |

Remote presets (`--remote-preset`, `pixel config remote-preset <preset>
[--model --base]`): `openrouter` (default), `ollama`, `local`, `deepseek`,
`opencode-go`, `openai`, `jev`, `clef-ollama`, `clef-cloudflare`. A preset picks
the endpoint, the default model and the key variable (`key_env`): `local` needs
no key, `clef-ollama` needs one only for a remote Ollama host.

- `openai` posts to `/v1/decisions` (default model `gpt-6-luna`) and reads
  `answers[0].probabilities`; a `refusal` answer is reported as `remote
  decisions refused the decision: <reason>`, with `(no reason given)` when it
  carries none. No hint is added.
- `jev` speaks TypeSafe's `/v1/systemone` with native probabilities;
  `clef-ollama` and `clef-cloudflare` serve Clef-flash through Ollama or
  Workers AI (`decide_clef`).
- `--if-warm`: answer only from a local engine already listening. It never
  starts it and never falls back to remote; otherwise it exits 1 with `pixel:
  not warm: …` on stderr and nothing on stdout, except that with `--json` the
  stdout is the failure envelope (`{"ok":false,…}`).

## Keys

A remote key resolves in this order: the preset's environment variable, then
`remote_keys.<preset>` of `~/.pixel/config.yaml` (written by `pixel config
remote-key <preset>`, value from stdin with `-`, file created 0600), then an
Infisical project when `INFISICAL_TOKEN` (or `PIXEL_INFISICAL_TOKEN`) and
`PIXEL_INFISICAL_PROJECT_ID` are set. An Infisical key is read once per
invocation and never stored; a configured lookup that fails fails the decision
loudly instead of falling back to "no key".

A key never appears in output, error text, JSON documents or the action log:
`logged_args` replaces every positional after `config remote-key <preset>` with
`<redacted>` (`config remote-key openai <redacted>` is what
`.pixel/actions.jsonl` holds). **Free text is not masked.** The state text given
to `pixel classify "<text>"` is an argument and is recorded as typed in
`actions.jsonl`; `--jsonl` carries it on stdin and records none of it.

## Input and output

- One-shot: `pixel classify <text> --label a,b [--context …] [--criterion
  label="…"] [--json]`. `--jsonl` serves one decision per stdin line, each a JSON
  object `{text, context?, labels, criteria?}`, and writes one line per input.
  A blank line produces no output; a malformed or invalid line answers
  `{"ok":false,"error":…}` and the stream goes on (exit stays 0), because one
  bad item must not abort a run.
- `--task-intent` judges the text with the built-in labels `bugfix`, `feature`,
  `refactor`, `investigate`, `question`, `review`, `ops` and names the commands
  that fit (only those whose plugin runs). The verdict is a classifier claim,
  never a repository fact. The brief does not use this flag: it sends the same
  labels plus `none` in its own `--jsonl` request.
- Every component (state, context, each criterion) is capped separately at
  `TEXT_CAP_CHARS` (32,768 characters) so a long context cannot erase a
  criterion. A cap that fired is disclosed: `marker`/`epistemics.confidence`
  `capped`, a `warning`, and the affected fields in `basis`; otherwise
  `complete`.
- A decision document carries `predicted`, `probs`, `epistemics` (`closed_world:
  false`, `lower_bound: false`, `basis`), and `snapshot` (`model`,
  `temperature: 0.0`, `labels`, `deterministic: false` for the remote engine,
  `provider`). A `--jsonl` result adds `ok: true`.
- A failing one-shot call prints `pixel: <error>` and exits 1; under `--json` the
  stdout is a parsable failure envelope.

## Where the data goes

With the switch on and a remote engine, the state text, context, labels and
criteria are sent to the configured provider. The brief makes that call for
the typed prompt of a `weak` or `prose` brief only (`brief.md`), through the same
configured engine and with no per-call override. That is the one place a hook
can contact a provider, and only after `pixel config classify on`.

## Not specified here

`pixel classify-eval` (the offline go/no-go evaluation) and `pixel
classify-history` (the verified-history store used by `--history`) are
documented in ARCHITECTURE.md, "Command surface".

## Tests that prove it

`config_cli` (`classify_off_should_block_every_engine_and_batch_before_reading_input`,
`classify_should_be_opt_in_even_when_an_engine_or_credentials_are_present`,
`config_remote_key_openai_round_trips_and_clears`, `classify_accepts_the_openai_preset_and_names_its_key_when_missing`),
`classify_cli`, the wire fixtures and `serve_line` tests in `classify.rs` and
`decide_*.rs`, `logged_args` tests in `main.rs` (key masking read back from the
log), `intent` tests (the brief's request and its fail-open modes).
