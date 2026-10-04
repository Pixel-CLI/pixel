// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Turns the runs scripts/record-demo.sh wrote into the traces the
// AgentDemo composition replays: src/demo/{vanilla,pixel}.json.
//
// For each arm it keeps the run with the median wall time, never the best
// one, and writes every tool call with the moment it was issued, the moment
// its result came back, and the size of that result: what reached the
// agent's context, counted as UTF-8 bytes divided by four like every other
// figure on the site.
//
// It also writes src/demo/runs.json, every run of both arms in the same
// shape, and copies the recording's meta.txt, so anyone can check that the
// replayed run is the median and what the others looked like.
//
// Usage: bun scripts/trace.ts <runs-dir>

import {copyFileSync, readdirSync, readFileSync, writeFileSync} from 'node:fs';
import {join} from 'node:path';

type Line = {t: number; e: any};

export type Step = {
	at: number; // ms from the run's first event, when the call was issued
	done: number; // ms when its result came back
	tool: string;
	label: string;
	tokens: number; // result size, bytes / 4
	lines: number;
};

export type Trace = {
	arm: string;
	run: string;
	model: string;
	durationMs: number;
	costUsd: number;
	turns: number;
	steps: Step[];
	answer: string;
	runs: {run: string; durationMs: number; costUsd: number; calls: number; tokensRead: number}[];
};

const dir = process.argv[2];
if (!dir) throw new Error('usage: bun scripts/trace.ts <runs-dir>');
const bytes = (s: string) => Buffer.byteLength(s, 'utf8');
// Paths as the agent saw them, relative to the repository it ran in.
let root = '';
const rel = (s: string) => (root ? s.split(root + '/').join('').split(root).join('.') : s);

const text = (content: unknown): string =>
	typeof content === 'string'
		? content
		: Array.isArray(content)
			? content.map((c: any) => (c.type === 'text' ? c.text : '')).join('')
			: '';

const label = (name: string, input: any): string => {
	switch (name) {
		case 'Bash':
			return '$ ' + rel(String(input.command)).split('\n')[0];
		case 'Read': {
			const range = input.offset ? ` :${input.offset}${input.limit ? `+${input.limit}` : ''}` : '';
			return 'read ' + rel(String(input.file_path)) + range;
		}
		case 'Grep':
			return `grep "${input.pattern}"${input.path ? ' ' + rel(String(input.path)) : ''}`;
		case 'Glob':
			return `glob ${input.pattern}`;
		default:
			return name;
	}
};

const parse = (file: string): Omit<Trace, 'arm' | 'runs'> => {
	const lines: Line[] = readFileSync(file, 'utf8')
		.split('\n')
		.filter(Boolean)
		.map((l) => JSON.parse(l));
	const t0 = lines[0].t;
	root = lines.find((l) => l.e.type === 'system' && l.e.subtype === 'init')?.e.cwd ?? '';
	const open = new Map<string, Step>();
	const steps: Step[] = [];
	let answer = '';
	let model = '';
	let result: any = null;
	for (const {t, e} of lines) {
		if (e.type === 'system' && e.subtype === 'init') model = e.model;
		if (e.type === 'assistant') {
			for (const c of e.message.content) {
				if (c.type === 'tool_use') {
					const step: Step = {at: t - t0, done: t - t0, tool: c.name, label: label(c.name, c.input), tokens: 0, lines: 0};
					open.set(c.id, step);
					steps.push(step);
				}
			}
		}
		if (e.type === 'user') {
			for (const c of e.message.content ?? []) {
				if (c.type !== 'tool_result') continue;
				const step = open.get(c.tool_use_id);
				if (!step) continue;
				const body = text(c.content);
				step.done = t - t0;
				step.tokens = Math.round(bytes(body) / 4);
				step.lines = body ? body.split('\n').length : 0;
			}
		}
		if (e.type === 'result') result = e;
	}
	if (!result) throw new Error(`${file}: no result event, the run did not finish`);
	if (result.is_error) throw new Error(`${file}: the run ended in error`);
	answer = rel(String(result.result ?? ''));
	return {
		run: file.split('/').pop()!.replace('.jsonl', ''),
		model,
		durationMs: result.duration_ms,
		costUsd: result.total_cost_usd,
		turns: result.num_turns,
		steps,
		answer,
	};
};

const sum = (steps: Step[]) => steps.reduce((a, s) => a + s.tokens, 0);
const out = join(import.meta.dir, '..', 'src', 'demo');
const all: Record<string, Omit<Trace, 'arm' | 'runs'>[]> = {};

for (const arm of ['vanilla', 'pixel']) {
	const runs = readdirSync(dir)
		.filter((f) => f.startsWith(arm + '-') && f.endsWith('.jsonl'))
		.map((f) => parse(join(dir, f)))
		.sort((a, b) => a.durationMs - b.durationMs);
	if (runs.length === 0) throw new Error(`no ${arm} runs in ${dir}`);
	all[arm] = runs;
	const median = runs[Math.floor((runs.length - 1) / 2)];
	const trace: Trace = {
		arm,
		...median,
		runs: runs.map((r) => ({run: r.run, durationMs: r.durationMs, costUsd: r.costUsd, calls: r.steps.length, tokensRead: sum(r.steps)})),
	};
	writeFileSync(join(out, `${arm}.json`), JSON.stringify(trace, null, '\t') + '\n');
	console.log(
		arm,
		runs.map((r) => `${r.run}: ${(r.durationMs / 1000).toFixed(1)} s, ${r.steps.length} calls, ${sum(r.steps)} tok read`).join(' | '),
		'-> median',
		median.run,
	);
}

writeFileSync(join(out, 'runs.json'), JSON.stringify(all, null, '\t') + '\n');
copyFileSync(join(dir, 'meta.txt'), join(out, 'meta.txt'));
