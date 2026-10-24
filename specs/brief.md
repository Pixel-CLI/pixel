# The brief

Status: normative for `pixel-brief`. Starting points in Pixel (`e8fd304`):
`crates/pixel/src/execution_brief/` (32k lines, of which ~14.7k production).
The crate reads no store: `chain::Evidence` is the seam, the daemon implements it.
The modules are `prompt` (signal), `tokens` (the task tokenizer), `relevance` and
`gate_model` (gate), `routes` (kind), `chain` (the recipes, the worker, render),
`kinds` and `locator` (the kinds' own packs), `answer` (receipt and excerpts),
`region_pack`, `parts`, `sent`, `session_paths`, `french`, `continuation`,
`git_changes` and `decision` (the journal's `brief` record).

## Pipeline

A pure lookup that resolves to exactly one definition omits the region pack's
usage blocks. Modification intent or an explicit request for usages, callers
or references keeps those blocks. Multiple targets and targets without a
definition retain the ordinary packing behavior; this does not change graph
retrieval or add a legacy command alias.

```
prompt ──► signal ──► gate ──► route (kind) ──► recipe (typed steps) ──► pack ──► render ──► deliver (parts)
                                   │                                                  ▲
                                   └── session dedupe (sent.rs) ───────────────────────┘
```

All stages are pure functions over an `Evidence` trait (one implementation over
the stores the daemon holds in memory; an in-process implementation for the
no-daemon path). A deadline (default 3 s) bounds the whole pipeline; each step
carries its own budget.

## Signal and gate

- **Typed text.** The user's text only: the last paragraph of a long untagged
  paste, a `<pasted_content>` block judged on the head of its first paragraph
  and the tail of its last (300 and 299 characters, links dropped), the text
  under Codex's `## My request:` heading. Envelopes (`<task-notification>`,
  command wrappers) and acknowledgements start no brief.
- **Signal** (`prompt.rs`): `Strong` (a backticked name, `snake_case`,
  `camelCase`, a path, a source file), `Weak` (a code word or opener), `Prose`
  (two or more content words, not an ops-only request: git, a PR, a release, a
  steer such as `go next`).
- **French.** A prompt the lexicon reads as French is searched on the English
  words code uses (`pixel-model` `terms` and the `fr-en.tsv` lexicon); the typed
  text is never rewritten.
- **Gate** (`relevance.rs`, `gate_model.rs`): a fixed logistic model over five features
  (`struct_per_mille`, `question`, `ops_share`, `struct_ratio`, `coverage`)
  with two thresholds that cut the score into `high`, `low` and `off`. The
  coefficients and thresholds are lifted byte-identical from `gate_model.rs`
  together with its parity vectors (`gate_parity.json`: blocks recorded from a
  daemon, with the score and tier the reference scorer gives them). The features read the
  daemon's keyword and co-file weights (the daemon's `relevance`,
  `keyword_weight` byte-identical); the gate is refit only by script, never by hand.
- **No model.** Pixel's classify intent judge is not consulted. `Strong`
  prompts skip the gate's refusal; `Weak` and `Prose` prompts need the gate.
- **Tiers.** `high`: the full brief with `confidence: high — n/m key terms
  covered`. `low`: ≤3 `path:line — text` files within 700 bytes, no recipe.
  `off`: nothing.

## Route: nine kinds

`Lookup`, `Flow`, `Tests`, `Config`, `Bugfix` (a defect or a change to a named
symbol), `Rationale` (why, a regression, a failure), `Overview`, `History` (when,
why, by whom, which commit, asked in so many words) and `Review`. Chosen from the
typed text by word lists (`routes.rs`), never by a model: Pixel's `Feature` kind
existed only for a classify verdict and its `targets_facts` op (task contracts),
so it is not ported.

Routing details found against the 13 `single` arena questions (ripgrep, 2026-10):
`what does the code do with X` is a question about X, not a tour of the repository;
a quoted commit title (`the commit titled '…'`) is a history question; a
`<crates> … depend on` question asks for the member list; an upper-case
`_`-joined token (`RIPGREP_CONFIG_PATH`) is a config question, since its words
are anchor segments and route nothing by themselves; and a review that names its
commit (a hex id, or a quoted title found in the log by its exact subject)
reviews that commit, not HEAD.

## Recipes

A recipe is an ordered list of typed steps with budgets and a maximum of
`MAX_OPS = 6` evidence ops. Shared prefix for every kind: literal search →
concept fallback → symbol pick. Then, by kind:

| Kind | Steps (Rust ports of the named Pixel probes) |
|---|---|
| Lookup | `name-files`; definition body (`pack-context`) |
| Flow | `flow-rank`, call chain (`evaluate`/trace witness hops) |
| Tests | `test-bodies`, callers filtered to test files |
| Config | `config-registry`, config locator lines, `list-signatures` of a named file |
| Change | impact (upstream, depth 1) when the prompt has change intent; definition body; callers excerpt; tests |
| Overview | `workspace-overview`, `overview-answer-evidence`, member manifest (`cand-k-member-manifest`) |
| History | `history-fallback`, commit patch (`cand-k-commit-patch`), history search |
| Review | `what-changed` + impact, git diff evidence |
| any | `error-origin` and `doc-sections` when the text asks about an error message or a documented behaviour; `cand-j-stop-cause` when it asks why something stops, skips or ignores |

`doc-sections` and the `rank-blocks`/`prune-noise` rules become render passes
(order blocks by shared words, drop blocks that share none). Probe `when_prompt`
regexes are not ported (overfit: `flow-rank` matched "undo/redo keyboard
shortcut").

**Steps run only on a word that asks.** A step that reads files checks a cheap
lexical condition on the typed text before it lists the tracked files: a member
noun or a layout word for `workspace-overview`, the word `crate` or `library`
for `overview-answer-evidence`, `the X crate` for `cand-k-member-manifest`, a
caller word for `cand-j-callers` (and a function name the graph resolves), an
`ALL_CAPS_NAME` or "where … used/read/applied" with a flag, option or setting
for `cand-k-code-sites`, a config word (setting, option, variable, flag, port,
default, max, min, …, only for the config kind) for `config-registry`, a
question about tests ("which tests cover …", only for the tests kind) for
`test-bodies`, the word "why" followed by an effect word (stops, skips, ignores, hides, suppresses, refuses, never, silently, missing, drops, fails, not; not the overview, review and history kinds) for `cand-j-stop-cause`, "trace", "call chain", "end to end", "walk through" or "from A through"
for `cand-j-chain` (and an entry function and a type the graph resolves),
a code name in backticks, dotted or `snake_case` for `flow-rank` (flow kind only), a quoted message of twelve characters or more, or an error word (error, exception, message, warning, alert, panic) for `error-origin` (not the history and review kinds), a history word (commit, changelog, git history, introduced, blame, release, tag) for `history-fallback` and `cand-k-commit-patch` (history kind only, and only when the brief holds no commits), a word about parts, functions, modules, files
or dependencies for `name-files` (the scope it was validated on), and at least
two content words that no source
question vetoes for `doc-sections` (never a lookup, a tests question or a
bugfix; not a prompt that names a constant, a caller or a `snake_case`
identifier unless the kind is overview or config). These conditions keep the
brief from paying for a file listing; what a step answers is decided by the
files it reads, and a repository with nothing to say gets nothing. They are not
Pixel's flow `when_prompt` regexes, which also decided *what* a probe answered.

**A step can open a closed gate.** The three Overview steps, `name-files`,
`cand-j-callers`, `cand-j-chain`, `cand-j-stop-cause`, `flow-rank`, `history-fallback`, `cand-k-commit-patch`, `cand-k-code-sites` and `test-bodies` ran in Pixel even when
the relevance gate had closed the prompt
(`when_gate_closed`), because a question about the workspace or about a module
shares few words with the code. Here a prompt the gate would close is briefed
anyway when one of those steps produced blocks for it: the gate decision is
logged as `open` with the reason `steps: answered from the manifests`, and the
brief carries the steps' blocks alone (no files from the refused words).
`doc-sections` never opens a gate.

**Deviation from the scripts.** `doc-sections` scores a section on its own
heading and opening lines. The script read the twelve lines after a heading
whatever came next, so a parent heading scored on its children's words and took
the slot of the child it contained.

**What a step takes away.** A step that found what a question asks for can
take blocks out of the rest of the pack (`steps::Drop`, read from a block's head
and, for tests, its lines; a step's own blocks are never dropped): the
manual and the whole-file blocks of the code `cand-k-code-sites` found, the
config registry and every block that names none of the stems it matched, the
tests and the definitions of other functions when `cand-j-callers` answered a
callers question, everything outside the member `cand-k-member-manifest` named, and the pack's own
call trace and call-graph facts when a flow step showed the chain that carries
the question's words (`Drop::CallTrace`), and every block that is not about commits when `cand-k-commit-patch` showed the commit (`Drop::AllButHistory`), and every ranged block (`path:a-b — label`) that is not a definition when `cand-j-stop-cause` answered (`Drop::NotDefinitions`).

**Flow steps.** `cand-j-chain` answers "trace how A turns into a T, from E
through …": the entry is the function the question names after "from" (else the
first the graph resolves), the target the type it names, and its constructors
the functions of the type's file whose return type is the type. The shortest call
path from the entry to a constructor (`Evidence::call_path`, the graph's
`trace`, depth 8) is shown hop by hop, at the call site when the hop is long and
whole when it is short, then the other callees of the entry that take the type,
then the callees at each hop whose names carry the question's words, two levels
deep. A hop reached by a call matched by name is drawn `~>` and the block says so. A path with no constructor, or none between the two, leaves the pack alone.
`flow-rank` follows a named entry (`client.fetch`, `handle_event`) through its
callees to depth 3: each callee is scored on the question's words in its name
(generic words weigh little) and path, a test or polyfill path scores nine below,
the passing ones of a level are followed (the best three when fewer than two
pass), and the entry and at most twelve ranked bodies are shown. Not ported: the
lanes of `flow-rank` for keyboard shortcuts, request routers and event delivery,
which read the benchmark frameworks' own shapes, and the sibling-method rule of
`cand-j-chain`, which needed qualified names the graph reads here do not carry.

**Stop cause.** `cand-j-stop-cause` answers "why does X stop, skip or ignore …"
with the lines where the question's rare words meet, and the tests that pin it
down. Every question word of four letters or more that is no function word or
framing word ("tests", "prove", "behaviour", "decides", …) is a group when it
occurs in at least one and at most half of the tracked source files, with the
verb synonyms of the script (`stop` = `quit` = `halt` = `abort` = `terminate` =
`bail` = `cancel`; `skip` … `exclude`; `fail` = `error` = `crash` = `panic`;
`print` = `display` = `output` = `emit`); a group weighs the square of the log
of how rare its word is. Three groups are needed. Every 30-line window of every
source file that holds the two heaviest groups scores `Σ weight × (1 + ln
count)`; windows inside a multi-line string literal (more than a tenth of the
lines), with less than 0.35 code lines, or mostly inside test units are left
out. The best window of each file, at most five files and 5 300 bytes, is
recentred on the lines that hold group words (a tie rounds to even), cut to
them with two lines of context (eight at least) and snapped over a comment run
above or an open brace below; a file named after a group word weighs 1.15 and a
file the brief points at 1.3. The first caller of a function of at most 13
lines whose name carries a group word is added when the graph answers
(`Evidence::symbols`, `Evidence::callers`), as the lines from 14 before the
call to 12 after it within the caller; a graph that fails adds nothing. When the
question also says test, spec, prove, proof or cover, the best test units follow
(a unit holds two groups; three files, one unit per file and two in a file
named after a group word, with the siblings of its family by name, at most
three; 12 lines and 3 000 bytes in all). Blocks are headed `path:a-b — where
the question's words meet (code|test)`, lines numbered, cut at 200 characters.
It opens a closed gate and takes away every non-definition block of the pack
(`Drop::NotDefinitions`: the label after the range contains `definition` or
`named by the prompt`, the script's test). Deviations from the script: the brief
the script read is not available to a step, so "the brief points at the file"
(boost 1.3) is decided per file from the paths the brief names, and a window the
brief's definition blocks already show is not skipped (the render's overlap
rules apply); the first caller is the first by caller path and call line, not
by caller declaration line, and is cut to the caller's span when the graph has
it; a question word is one group whatever its case; the dead checks of the
script (a window of two groups, a score above zero) are not repeated.

**History steps.** A history question whose brief holds no commits (the history
index missed, or is absent) is answered from git (`Evidence::git_read`: `log`,
`show`, `rev-list` and `tag` only, bounded in bytes and time). The commit is the
first that matches: a version the prompt quotes (its release tag, then subjects
that share a word stem with the rest of the prompt); a quoted title through
`--grep`; the subjects that share three of the prompt's words, or two adjacent in
the prompt, weighted by how rare each is in the last 20 000 subjects; the
prompt's rarest words through `--grep --all-match` and, for `history-fallback`
only, a `-S` pickaxe. `history-fallback` prints the candidates, then the
top commit's message, stat, patch (a merge against its first parent) and first
tag. `cand-k-commit-patch` prints the message (fenced output cut to the lines
that carry numbers), the complete `--stat` list, and the hunks file by file,
source first, manifests next, lockfiles summarised, within about 4 800 bytes. It
runs first and its block replaces the fallback's; the rest of the pack goes.
Deviations: headers carry the abbreviated sha where the script printed a full
one for a subject match; the hunk cut counts bytes where the script assumed sixty
a line.

**Error origin.** `error-origin` answers "where does this message come from"
with candidates labelled by what their syntax establishes. The message is a
quoted one found literally (the longest leading run of three words or more,
inside a string of a non-test source line, in no comment) or the string literal
that carries most of the prompt's words, each weighed by how few source lines
carry it, taking the whole string when it spans lines and the emitting call and
function into account; a literal found in a JSON message catalogue is followed to
the code that names its key. The block lists the nearest diagnostic emitter
(`eprintln!`, `bail!`, `panic!`, `throw new Error`) whose function carries two
salient prompt words, the origin and its function, the lexical callers of that
function two levels up, and one test that calls it by name; every call is marked
as a candidate whose receiver is unresolved. It is an append block: it opens no
gate. Not ported: the lanes that read a Python AST (caught types, raises, import
boundaries, exact roles: a Python line's role is read from its shape), the
default-404-handler lane (one web framework's callbacks and package manifest),
and the emitters of one tool (`print_error!`) and one framework (`next(err)`).

**Parts of a script that are not ported.** `cand-j-callers` carried a second
engine for JavaScript re-exports and import aliases and two frameworks'
request-context special cases; they worked around a graph that did not resolve
imports and read as the benchmark's own questions, so only the generic core is
here (callers, their callers, helpers named after the function, a unit test
beside each callee file, the tests that set the environment names it reads). The
replay says whether a caller question still misses callers on a JavaScript
repository. `config-registry` keeps the registry (environment reads in five languages,
typed environment declarations, exported constants, dev-server ports, literal
`set("key", value)` calls), its ranking, the candidate list for a key declared
twice and the masking; it drops the lanes that read particular frameworks'
internals (a Python AST loader chain, compiled-setting and CLI-argument scans,
browser storage keys) and every `.env*` file, templates included: the
credential-path rule of the brief refuses them. `test-bodies` finds the test files that carry the subject words with
one bounded search of the text index (the script counted matching lines with
`rg -c`). `cand-k-code-sites` keeps the script's budget, ranking and caps; its
searches are index searches, capped at 4 000 rows, and a stem that appears in
more than twice the allowed number of files is dismissed before any file is read.

**The render passes** (`passes.rs`) run on the pack's candidate blocks, never on a
step's own. `rank-blocks` scores each block by the question's words its head
names, drops the blocks that name none (when some block does: it never empties a
pack) and moves the rest to the front by score; it leaves the overview, review,
flow and history kinds alone. `prune-noise` drops lists of test names when test
bodies are shown, shell-completion files unless the question names completions,
joins overlapping ranges of one file, takes from a whole-file region the lines
another block of the file shows, and cuts a block above 6 000 bytes (18 lines for
a test) to the window with the most question words. Not ported: the call-trace
rewrite and the config-registry row filter (the Rust steps print neither shape),
the documentation-section drop (`doc-sections` gates itself) and the 8–14 KB
total (the pack has its own `parts × 9 800` budget and priorities).

**Ablation.** Every step has a flag (`workspace-overview`,
`overview-answer-evidence`, `cand-k-member-manifest`, `cand-j-callers`,
`cand-k-code-sites`, `cand-j-chain`, `cand-j-stop-cause`, `flow-rank`, `history-fallback`,
`cand-k-commit-patch`, `error-origin`, `config-registry`, `test-bodies`, `doc-sections`, `name-files`, `rank-blocks`, `prune-noise`; the names of the probes they
replace). `pixel brief --without name[,name]` leaves steps out of one brief,
and `PIXEL_BRIEF_STEPS=-name,+name` does the same for the process that computes
it; the flag is applied after the variable. The brief's journal line lists the
steps that ran with their milliseconds (`steps`, in the order they finished):
a step whose lexical gate stayed closed is absent, not `0`; the tracked-file
listing is counted by the first step that asks for it; the two render passes
are inside the pack's `rank_ms`. The external replay compares arms
with and without each step; a step that does not remove tool calls is deleted.

## Third-party probes

Pixel bundles no probe; a plugin with the `brief-probe` capability (`plugins.md`)
adds blocks to the brief. After the brief is rendered, every probe that is on,
trusted and due runs in parallel: its `when_kinds`/`skip_kinds`/`when_prompt`
fit the question, and on a prompt the relevance gate closed only the probes that
set `when_gate_closed` run, and may answer it. Each gets
`{prompt, repo_root, brief, kind, session_files}` as JSON on stdin and has
`budget_ms` (at most 2 500) inside what is left of the brief's window; a hook
gives probes at least 1.2 s after the chain and none runs past 5 s from the
prompt, the `brief` command gives none beyond its window. What a probe prints is
a list of blocks (`### header`, then lines) and the directives `drop: <regex>`
(remove the brief's own blocks whose header matches) and `first: <regex>` (move
them to the front). `lead` blocks go before the brief's, `append` blocks after;
a block whose header the brief already holds is skipped; the oracle footer stays
last; over the part budget the append blocks go first, then the brief's last
blocks, then the lead ones. A probe that times out, fails or prints nothing is
skipped and journalled (`probes`: name, ms, bytes, `ok|empty|timeout|failed|skipped`;
`stages.probe_ms`), and the brief is never worse for it. The `brief` command's
merge is not used when it would exceed 3 990 bytes. `PIXEL_BRIEF_PROBES=0|off|false`
runs none. The merge is pinned by `crates/pixel-brief/tests/fixtures/probe-merge.json`.

## Known weak spots to fix with their own prototype

From the 0.9.0 evidence: overview (no brief in any run), history (−3% tokens),
tests (+1%), and incompleteness (12 of 34 brief answers in the public acceptance
review were incomplete: half-covered test suites, flows missing callers). Each
is prototyped in a throwaway script (`prototype-first`) before its recipe step
changes.

## Pack, render, deliver

- **Pack** (`pack.rs`): region pack of definitions with docs and bodies, usage
  sites, tests, and locator blocks; ranges already sent in this session are
  left out (`sent.rs`); the whole pack is bounded by `parts × 9,800` characters.
- **Render** (`render.rs`): `[PIXEL:BRIEF]` header, `searched:` and `confidence:`
  lines, `read:`, `also:` and `files:` entries as `path:line`, the staleness
  line when any evidence predates the last edit, the receipt of what was
  searched, and the directive footer when tier is high and evidence complete.
  The 0.9.0 `SKILL_LINE` is dropped: it pointed the model at a CLI it does not
  call.
- **Deliver** (`parts.rs`): per-host limits and the `--part K/N` split from
  `hosts.md`. `pixel-brief` owns the line-boundary splitter (`parts::split`); the
  store parts 2..N wait on (`.pixel/brief-parts/`) belongs to the hook handler
  (`hosts.md`, "Size limits").
- **Staleness.** The brief reads with `stale_ok`; a successful response with
  `epistemics.staleness_ms` marks the evidence stale and the rendered brief
  says so once. A stale pack is a lead to verify.

## Third-party probes

`brief-probe` plugins (`plugins.md`) keep running: user tier and trusted repo
tier, in parallel with a cleared environment inside `min(budget, deadline)`,
input `{prompt, repo_root, brief, kind, session_files}` on stdin, output
`### header` blocks plus `drop:` and `first:` directives. Pixel bundles none.

## Journal

One typed record per decision into `.pixel/journal.jsonl` (`metrics.md`): signal,
gate (`open`/`closed`/`declined`), tier, score, kind, steps run with their
milliseconds, bytes, whether the pack was cut, deadline, route (`daemon` or
`local`), and the masked typed text and its SHA-256.

## Tests that prove it

Gate parity with `gate_reference.py` over the 218 labelled prompts; each step's
output on a fixture repository; `pixel brief --json` byte-stable across runs;
deadline expiry yields a partial brief marked cut; the part splitter
concatenates back to the stored brief; French prompts route as their English
equivalents. Acceptance replays the 13 public questions against Pixel 0.9.0
(`roadmap.md`).
