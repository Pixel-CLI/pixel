claim: readme-read-savings-median
date: 2026-10-18T13:21:30Z
command: scripts/bench/bench-read-savings.sh
exit: 0
--- pixel --version
pixel 0.8.0
commit: 9a8adca44865008d7a2d63bd6d8f0565a640ba27
target: aarch64-apple-darwin
rustc: rustc 1.99.0 (b940084d7 2026-09-28)
built: 2026-10-15
--- output
pixel 0.8.0
name	repository	path	commit	lines	full_tok	pixel_tok	saved_pct	signatures	status
transformers	huggingface/transformers	src/transformers/trainer.py	98d3982	4639	57058	1649	97.1	90	kept
fastapi	fastapi/fastapi	fastapi/routing.py	d623544	6447	63908	2237	96.5	150	kept
next	vercel/next.js	packages/next/src/server/base-server.ts	52788bd	3459	29454	964	96.7	67	kept
langchain	langchain-ai/langchain	libs/core/langchain_core/language_models/chat_models.py	ed0ad74	2748	27493	876	96.8	65	kept
django	django/django	django/db/models/query.py	6ade625	3137	30449	2453	91.9	186	kept
cpython	python/cpython	Lib/typing.py	de2ea9a	3955	34829	3386	90.3	251	kept
vscode	microsoft/vscode	src/vs/editor/common/model/textModel.ts	227803b	2745	27255	5531	79.7	217	kept
tokio	tokio-rs/tokio	tokio/src/runtime/scheduler/multi_thread/worker.rs	7bb6f07	1566	13995	1064	92.4	66	kept
react	facebook/react	packages/react-reconciler/src/ReactFiberWorkLoop.js	cbb046a	5683	50936	324	99.4	20	excluded
median saved (8 kept files): 94.5%
