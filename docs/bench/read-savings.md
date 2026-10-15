# Reading well-known files: full read vs `pixel list-signatures`

What reaches an agent that needs to know what a large file contains. The
baseline reads the whole file; the Pixel arm runs `pixel list-signatures
<file>`, the file's skeleton (one line per definition, with its line). The measured quantities are file and signatures-output UTF-8 byte volumes.
Both sides estimate tokens from those bytes divided by four, rounded down, as the "Reading code" table on
`/benchmarks/` and shunt's benchmark do. No second model, no agent run.

- **Command:** `scripts/bench/bench-read-savings.sh` (downloads each file at its
  pinned commit into a throwaway repository under `$TMPDIR`).
- **Binary:** pixel 0.5.0, September 2026.
- **Files:** ten large, well-known files of popular projects, listed for
  recognition across languages (Python, TypeScript, Rust, JavaScript)
  before any saving was measured, each pinned to the last commit that
  touched it on 2026-09-25. One was dropped from the table: React's
  `ReactFiberHooks.js`, a second React file with the same Flow failure
  (19 signatures, 99.4%); `ReactFiberWorkLoop.js` stays in it, excluded,
  to show the case.

## Raw output

```tsv
name	repository	path	commit	lines	full_tok	pixel_tok	saved_pct	signatures	status
transformers	huggingface/transformers	src/transformers/trainer.py	98d3982	4639	57058	1649	97.1	90	kept
fastapi	fastapi/fastapi	fastapi/routing.py	d623544	6447	63908	2237	96.5	150	kept
next	vercel/next.js	packages/next/src/server/base-server.ts	52788bd	3459	29454	832	97.2	60	kept
langchain	langchain-ai/langchain	libs/core/langchain_core/language_models/chat_models.py	ed0ad74	2748	27493	876	96.8	65	kept
django	django/django	django/db/models/query.py	6ade625	3137	30449	2453	91.9	186	kept
cpython	python/cpython	Lib/typing.py	de2ea9a	3955	34829	3386	90.3	251	kept
vscode	microsoft/vscode	src/vs/editor/common/model/textModel.ts	227803b	2745	27255	5531	79.7	217	kept
tokio	tokio-rs/tokio	tokio/src/runtime/scheduler/multi_thread/worker.rs	7bb6f07	1566	13995	1064	92.4	66	kept
react	facebook/react	packages/react-reconciler/src/ReactFiberWorkLoop.js	cbb046a	5683	50936	324	99.4	20	excluded
```

median saved (8 kept files): 94.5%

## Checking the parse

A skeleton that misses definitions looks like a bigger saving, so every row
carries the number of signatures listed, checked against the file's own
module- and class-level definitions:

| File | `def`/`class` at indent 0 or 4 | Signatures listed |
| --- | --- | --- |
| Transformers `trainer.py` | 89 | 90 |
| FastAPI `routing.py` | 143 | 150 |
| LangChain `chat_models.py` | 69 | 65 |
| CPython `typing.py` | 244 | 251 |
| Django `query.py` | 180 | 186 |

(`grep -c -E '^( {4})?(async def|def|class) ' <file>`; nested functions
are not part of an outline.) React's `ReactFiberWorkLoop.js` has 125
top-level `function` declarations (`grep -c -E '^(export )?(async
)?function '`) and lists 20: its Flow type annotations break the JavaScript
grammar, so its 99.4% is excluded from the range and the median. That is a
gap in Pixel's JavaScript support, not a result.

## Reading the numbers

- The spread is real: files dense with small methods (VS Code's
  `textModel.ts`, 217 signatures in 2,745 lines) save the least, because
  the skeleton itself is long.
- This compares two outputs for an outline question, not equivalent inputs
  for every coding task. Bodies, comments and subsequent reads are omitted;
  signature counts only provide a coarse parse check (LangChain: 65 versus
  69 declarations). Eight selected large files are not a representative
  repository or agent-workflow sample.
- Tokens are estimates, not tokenizer or provider usage. No agent duration
  or billed cost is measured. `pixel token-savings` records local Pixel
  output volumes and workflow estimates, not every later agent read.
- These archived Pixel 0.5.0 rows support the file/outline comparison only;
  they are not a remeasurement of the current release.
- The landing page's token wall shows the Transformers row; its caption
  quotes the range and median of the kept rows.
