// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {AbsoluteFill, interpolate, useCurrentFrame, useVideoConfig} from 'remotion';
import {C, F, PX} from './theme';

// Replays two recorded agent runs side by side on one clock: the same task,
// the same model, the same bare setup, one with Pixel's protocol. Every line,
// time and token count comes from src/demo/*.json, which scripts/trace.ts
// derives from the stream-json that scripts/record-demo.sh captured.

export type Step = {at: number; done: number; tool: string; label: string; tokens: number; lines: number};
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

export type DemoProps = {vanilla: Trace; pixel: Trace; speed: number; recorded: string; modelName: string};

export const FPS = 30;
export const LEAD = 45; // frames before the clock starts
export const HOLD = 45; // frames after both runs end, before the summary
export const SUMMARY = 180;

export const demoFrames = (vanilla: Trace, pixel: Trace, speed: number) =>
	LEAD + Math.ceil((Math.max(vanilla.durationMs, pixel.durationMs) / speed / 1000) * FPS) + HOLD + SUMMARY;

const W = 1600;
const PANE_Y = 214;
const PANE_H = 560;
const PANE_W = 740;
const LINE = 25;
const MAX_CHARS = 58;

const cut = (s: string, n = MAX_CHARS) => (s.length > n ? s.slice(0, n - 1) + '…' : s);
const k = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : `${n}`);
const clock = (ms: number) => {
	const s = Math.floor(ms / 1000);
	return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};
const readTokens = (steps: Step[], ms: number) => steps.reduce((a, s) => a + (s.done <= ms ? s.tokens : 0), 0);

/// The answer's first lines, without Markdown decoration.
const answerLines = (answer: string, n: number) =>
	answer
		.split('\n')
		.map((l) => l.replace(/[*`#]/g, '').replace(/^\s*[-\d.]+\s*/, '').trim())
		.filter((l) => l.length > 0)
		.slice(0, n);

type Row = {text: string; color: string; weight: number; indent: number};

const Pane = ({trace, x, ms, accent, title, done}: {trace: Trace; x: number; ms: number; accent: string; title: string; done: boolean}) => {
	const rows: Row[] = [];
	for (const s of trace.steps) {
		if (s.at > ms) break;
		rows.push({text: cut(s.label), color: C.ink, weight: 500, indent: 0});
		if (s.done <= ms) {
			rows.push({text: `↳ ${s.lines} lines · ${k(s.tokens)} tokens`, color: accent, weight: 500, indent: 22});
		} else {
			rows.push({text: '↳ …', color: C.inkSoft, weight: 400, indent: 22});
		}
	}
	if (done) {
		rows.push({text: '', color: C.ink, weight: 400, indent: 0});
		rows.push({text: `answer · ${clock(trace.durationMs)}`, color: accent, weight: 600, indent: 0});
		for (const l of answerLines(trace.answer, 4)) rows.push({text: cut(l, MAX_CHARS - 2), color: C.inkSoft, weight: 400, indent: 22});
	}
	const fit = Math.floor((PANE_H - 76) / LINE);
	const shown = rows.slice(-fit);
	const tabW = title.length * 15.5 + 60;
	return (
		<g>
			<rect x={x} y={PANE_Y} width={PANE_W} height={PANE_H} rx={8} fill={C.term} stroke={done ? accent : C.line} strokeOpacity={done ? 0.7 : 1} strokeWidth={1.5}/>
			<rect x={x + 24} y={PANE_Y - 20} width={tabW} height={40} rx={4} fill={accent}/>
			<rect x={x + 40} y={PANE_Y - 7} width={14} height={14} rx={2} fill={C.onGreen}/>
			<text x={x + 66} y={PANE_Y + 10} fontFamily={F.display} fontSize={30} fontWeight={700} style={PX} fill={C.onGreen} letterSpacing={1}>
				{title}
			</text>
			{shown.map((r, i) => (
				<text
					key={i}
					x={x + 26 + r.indent}
					y={PANE_Y + 52 + i * LINE}
					fontFamily={F.mono}
					fontSize={16.5}
					fontWeight={r.weight}
					fill={r.color}
				>
					{r.text}
				</text>
			))}
		</g>
	);
};

/// Tokens read so far, one square per `per` tokens: the site's token wall.
const Wall = ({x, y, tokens, per, color, cols}: {x: number; y: number; tokens: number; per: number; color: string; cols: number}) => {
	const n = Math.round(tokens / per);
	const size = 9;
	const gap = 3;
	return (
		<g>
			{Array.from({length: n}, (_, i) => (
				<rect key={i} x={x + (i % cols) * (size + gap)} y={y + Math.floor(i / cols) * (size + gap)} width={size} height={size} rx={1} fill={color}/>
			))}
		</g>
	);
};

const Meters = ({trace, x, ms, accent, per}: {trace: Trace; x: number; ms: number; accent: string; per: number}) => {
	const t = Math.min(ms, trace.durationMs);
	const calls = trace.steps.filter((s) => s.at <= t).length;
	const tokens = readTokens(trace.steps, t);
	const y = PANE_Y + PANE_H + 34;
	return (
		<g>
			<text x={x} y={y + 32} fontFamily={F.mono} fontSize={40} fontWeight={600} fill={C.ink}>
				{clock(t)}
			</text>
			<text x={x + 140} y={y + 12} fontFamily={F.mono} fontSize={14} fill={C.inkSoft} letterSpacing={1.2}>
				TOOL CALLS
			</text>
			<text x={x + 140} y={y + 36} fontFamily={F.mono} fontSize={22} fontWeight={600} fill={C.ink}>
				{calls}
			</text>
			<text x={x + 290} y={y + 12} fontFamily={F.mono} fontSize={14} fill={C.inkSoft} letterSpacing={1.2}>
				TOKENS READ INTO CONTEXT
			</text>
			<text x={x + 290} y={y + 36} fontFamily={F.mono} fontSize={22} fontWeight={600} fill={accent}>
				{tokens.toLocaleString('en-US')}
			</text>
			<Wall x={x} y={y + 60} tokens={tokens} per={per} color={accent} cols={61}/>
		</g>
	);
};

const Figure = ({x, y, label, left, right, better}: {x: number; y: number; label: string; left: string; right: string; better: string}) => (
	<g>
		<text x={x} y={y} fontFamily={F.mono} fontSize={16} fill={C.inkSoft} letterSpacing={1.4}>
			{label}
		</text>
		<text x={x} y={y + 66} fontFamily={F.mono} fontSize={60} fontWeight={600} fill={C.coral}>
			{left}
		</text>
		<text x={x} y={y + 140} fontFamily={F.mono} fontSize={60} fontWeight={600} fill={C.greenHi}>
			{right}
		</text>
		<text x={x} y={y + 180} fontFamily={F.mono} fontSize={18} fontWeight={600} fill={C.ink}>
			{better}
		</text>
	</g>
);

const median = (xs: number[]) => {
	const s = [...xs].sort((a, b) => a - b);
	const m = Math.floor(s.length / 2);
	return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};

/// The summary's headline follows the medians; it never assumes a win.
const headline = (dt: number, dtok: number) =>
	dt < 0 && dtok < 0 ? 'Faster, and fewer tokens.' : dtok < 0 ? 'Fewer tokens. Not faster.' : dt < 0 ? 'Faster, not lighter.' : 'No gain on this task.';

const delta = (a: number, b: number) => {
	const d = Math.round(((b - a) / a) * 100);
	return d <= 0 ? `−${-d}% with Pixel` : `+${d}% with Pixel`;
};

export const AgentDemo = ({vanilla, pixel, speed, recorded, modelName}: DemoProps) => {
	const frame = useCurrentFrame();
	const {durationInFrames} = useVideoConfig();
	const ms = Math.max(0, ((frame - LEAD) / FPS) * 1000 * speed);
	const end = durationInFrames - SUMMARY;
	const summary = interpolate(frame, [end, end + 15], [0, 1], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'});
	const totalA = readTokens(vanilla.steps, Infinity);
	const totalB = readTokens(pixel.steps, Infinity);
	// The replay is the median-time run of each arm; the summary gives the
	// median of every metric over all runs.
	const m = (t: Trace, f: (r: Trace['runs'][number]) => number) => median(t.runs.map(f));
	const timeA = m(vanilla, (r) => r.durationMs), timeB = m(pixel, (r) => r.durationMs);
	const tokA = m(vanilla, (r) => r.tokensRead), tokB = m(pixel, (r) => r.tokensRead);
	const costA = m(vanilla, (r) => r.costUsd), costB = m(pixel, (r) => r.costUsd);
	// Squares per token, so the larger run fills at most four rows.
	const per = Math.max(100, Math.ceil(Math.max(totalA, totalB) / (61 * 4) / 100) * 100);
	const task = 'retry a leased push when the remote branch moved: list the files to change';
	return (
		<AbsoluteFill style={{background: C.ground}}>
			<svg width={W} height={1000} viewBox={`0 0 ${W} 1000`}>
				<defs>
					<pattern id="grid" width="32" height="32" patternUnits="userSpaceOnUse">
						<rect x="1" y="1" width="2" height="2" fill="#ffffff" opacity={0.045}/>
					</pattern>
				</defs>
				<rect width={W} height={1000} fill="url(#grid)"/>
				<g opacity={1 - summary * 0.88}>
					<text x={60} y={92} fontFamily={F.display} fontSize={60} fontWeight={700} style={PX} fill={C.ink}>
						Same task. Same model. One has Pixel.
					</text>
					<text x={1540} y={62} textAnchor="end" fontFamily={F.mono} fontSize={15} fontWeight={600} letterSpacing={1.4} fill={C.inkSoft}>
						REAL RUNS · {speed}× SPEED
					</text>
					<text x={1540} y={88} textAnchor="end" fontFamily={F.mono} fontSize={15} letterSpacing={1.2} fill={C.inkSoft}>
						{modelName.toUpperCase()} · MEDIAN-TIME RUN OF {vanilla.runs.length}
					</text>
					<text x={60} y={146} fontFamily={F.mono} fontSize={19} fill={C.inkSoft}>
						<tspan fill={C.greenHi}>task ›</tspan> {task}
					</text>
					<Pane trace={vanilla} x={40} ms={ms} accent={C.coral} title="WITHOUT PIXEL" done={ms >= vanilla.durationMs}/>
					<Pane trace={pixel} x={820} ms={ms} accent={C.green} title="WITH PIXEL" done={ms >= pixel.durationMs}/>
					<Meters trace={vanilla} x={40} ms={ms} accent={C.coral} per={per}/>
					<Meters trace={pixel} x={820} ms={ms} accent={C.green} per={per}/>
					<text x={1560} y={990} textAnchor="end" fontFamily={F.mono} fontSize={12} fill={C.inkFaint}>
						one square = {per} tokens
					</text>
				</g>
				{summary > 0 && (
					<g opacity={summary}>
						<rect width={W} height={1000} fill={C.ground} opacity={0.9}/>
						<text x={W / 2} y={170} textAnchor="middle" fontFamily={F.display} fontSize={76} fontWeight={700} style={PX} fill={C.ink}>
							{headline(timeB - timeA, tokB - tokA)}
						</text>
						<g>
							<rect x={420} y={214} width={14} height={14} rx={2} fill={C.coral}/>
							<text x={444} y={227} fontFamily={F.mono} fontSize={16} fill={C.inkSoft}>without Pixel</text>
							<rect x={640} y={214} width={14} height={14} rx={2} fill={C.green}/>
							<text x={664} y={227} fontFamily={F.mono} fontSize={16} fill={C.inkSoft}>with Pixel</text>
						</g>
						<Figure x={250} y={320} label="MEDIAN WALL TIME" left={clock(timeA)} right={clock(timeB)} better={delta(timeA, timeB)}/>
						<Figure x={690} y={320} label="MEDIAN TOKENS READ" left={k(tokA)} right={k(tokB)} better={delta(tokA, tokB)}/>
						<Figure x={1130} y={320} label="MEDIAN API COST" left={`$${costA.toFixed(2)}`} right={`$${costB.toFixed(2)}`} better={delta(costA, costB)}/>
						<text x={W / 2} y={640} textAnchor="middle" fontFamily={F.mono} fontSize={16} fill={C.inkSoft}>
							{modelName}, bare setup on both sides, {vanilla.runs.length} runs per side started together, recorded {recorded}. Replayed: each side's median-time run.
						</text>
						<text x={W / 2} y={668} textAnchor="middle" fontFamily={F.mono} fontSize={16} fill={C.inkSoft}>
							Every run and its trace: docs/motion/src/demo/ in Pixel-CLI/pixel.
						</text>
					</g>
				)}
			</svg>
		</AbsoluteFill>
	);
};
