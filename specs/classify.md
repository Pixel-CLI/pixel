# classify

Status: normative for the opt-in `pixel classify` (not in the default path).
Lifted from `classify.rs`, `classify_setup.rs`, `decide_remote.rs`,
`decide_jev.rs`, `decide_clef.rs`, `decide_ollaya.rs`, `decide_infisical.rs`,
`config_cmd.rs`. Dropped: `classify-eval`, `classify-history`, the brief's
intent judge.

A zero-shot decision over a bounded label set: state text, shared `--context`,
labels with per-label `--criterion`; one probability per label, renormalised to
sum 1, plus the predicted label. Output discloses `snapshot.deterministic=false`
and `snapshot.provider`.

## Engines

- **remote** (default): an OpenAI-compatible chat completion; `--remote-preset
  openrouter|ollama|local|deepseek|opencode-go|openai|jev|clef-ollama|clef-cloudflare`
  picks endpoint and key variable, `--remote-model` the model. Probabilities
  are verbalised.
- **openai** preset: OpenAI's `/v1/decisions` wire (`gpt-6-luna`): a choice
  question built from labels and criteria, `answers[0].probabilities` renormalised.
  A refusal without reason is reported with a hint that the input may be too
  vague or the model lacks Decisions access.
- **jev**: TypeSafe's `/v1/systemone` with native calibrated probabilities.
- **clef-ollama / clef-cloudflare**: Clef-flash through Ollama or Workers AI.
- **ollaya**: a local decision daemon's typed-choice readout; `--if-warm`
  answers only from an engine already listening, never starts it, never falls
  back to remote, else exits 1 with empty stdout. Without `--label` the local
  engine answers its default question battery.

Remote keys resolve in order: the preset's env var, `pixel config remote-key
<preset>` (piped from stdin, never an argument), an Infisical project when
`INFISICAL_TOKEN` and `PIXEL_INFISICAL_PROJECT_ID` are set (read once, never
stored). Keys are masked in every sink (`metrics.md`).

`pixel config classify on|off` is the global kill switch (default off);
`classify-engine`, `remote-preset <preset> [--model --base]`, `remote-key`.
State, context and criteria are capped separately with disclosure. `--jsonl`
serves one decision per stdin line. `--task-intent` judges text with the
built-in coding-task labels (bugfix, feature, refactor, investigate, question,
review, ops) and names the `pixel` commands that fit the verdict; the verdict is a
classifier claim, never a repository fact.

`pixel config setup` (a terminal) asks whether the classifier is allowed and, when
it is, runs the engine proposal below; a failed first-time installation leaves
the classifier off. Nothing in the default path (the brief, the hooks, every
other command) calls a model or the network.

Setup proposes the engine with each option's measured accuracy and, when
accepted, the `pixel-classify` skill and the pi classify tools (`skills.md`,
`hosts.md`).

## Tests that prove it

Per-engine wire fixtures (request body and response parsing); renormalisation;
key resolution order; key masking read back from the action log; caps disclosed;
`--if-warm` never spawns; the default build makes no network call.
