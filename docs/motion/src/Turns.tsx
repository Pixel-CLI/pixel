// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

import {AbsoluteFill, interpolate, useCurrentFrame} from 'remotion';
import {C, F, PX} from './theme';

const W = 1600;
const H = 1000;

const rise = (frame: number, start: number, len = 14) =>
	interpolate(frame, [start, start + len], [0, 1], {extrapolateLeft: 'clamp', extrapolateRight: 'clamp'});

const Title = ({lines}: {lines: [string, string]}) => (
	<div style={{position: 'absolute', left: 80, top: 56, fontFamily: F.display, ...PX, fontWeight: 700, fontSize: 84, lineHeight: 0.95, color: C.ink}}>
		<div>{lines[0]}</div>
		<div style={{color: C.greenHi, textShadow: `0 0 28px ${C.green}88`}}>{lines[1]}</div>
	</div>
);

const Foot = ({text}: {text: string}) => (
	<div style={{position: 'absolute', left: 80, bottom: 40, fontFamily: F.mono, fontSize: 22, color: C.inkSoft}}>{text}</div>
);

type Block = {label: string; out: number; answer?: boolean; brief?: boolean};

// One lane of the timeline: a block per model turn, left to right. A block's
// base is the conversation re-sent on that turn (it grows as the session
// does); its top is what the turn's tool printed. `trim` shortens the
// printed part, which is all an output filter can do.
const Lane = ({
	frame, y, title, tag, color, blocks, trim = 1, start,
}: {
	frame: number; y: number; title: string; tag: string; color: string; blocks: Block[]; trim?: number; start: number;
}) => {
	const bw = 150;
	const gap = 22;
	return (
		<g transform={`translate(80, ${y})`}>
			<text x={0} y={-150} fontFamily={F.display} fontSize={44} fontWeight={700} fill={color} style={PX}>{title}</text>
			<text x={0} y={-120} fontFamily={F.mono} fontSize={20} fill={C.inkSoft}>{tag}</text>
			{blocks.map((b, i) => {
				const t = rise(frame, start + i * 16);
				const ctx = 16 + i * 5;
				const out = b.out * 0.7 * trim;
				const x = i * (bw + gap);
				const fill = b.brief ? C.green : color;
				return (
					<g key={i} opacity={t} transform={`translate(${x}, ${(1 - t) * 14})`}>
						<rect x={0} y={-ctx - out} width={bw} height={ctx + out} rx={3} fill={C.panel} stroke={b.answer ? C.greenHi : C.cellEdge} strokeWidth={b.answer ? 2.5 : 1.5}/>
						<rect x={0} y={-out} width={bw} height={out} rx={3} fill={fill} opacity={b.brief ? 0.9 : 0.75}/>
						<text x={bw / 2} y={26} textAnchor="middle" fontFamily={F.mono} fontSize={19} fill={b.answer ? C.greenHi : C.ink}>{b.label}</text>
					</g>
				);
			})}
		</g>
	);
};

const search: Block[] = [
	{label: 'prompt', out: 0},
	{label: 'grep', out: 46},
	{label: 'read', out: 96},
	{label: 'grep', out: 46},
	{label: 'read', out: 96},
	{label: 'grep', out: 46},
	{label: 'read', out: 96},
	{label: 'answer', out: 0, answer: true},
];

/// PIXEL vs RTK as a turn timeline. An illustration of the mechanism, not a
/// recording: no timing or token figure is drawn.
export const PIXELvsRTK = () => {
	const frame = useCurrentFrame();
	return (
		<AbsoluteFill style={{background: C.ground}}>
			<svg width={W} height={H} viewBox={`0 0 ${W} ${H}`} style={{position: 'absolute'}}>
				<rect width={W} height={H} fill={C.ground}/>
				<ellipse cx={W / 2} cy={H} rx={900} ry={330} fill={C.green} opacity={0.07}/>
				<Lane frame={frame} y={420} title="No tools" tag="every step is a turn; every turn re-sends the conversation" color={C.coral} blocks={search} start={20}/>
				<Lane frame={frame} y={650} title="+ RTK" tag="the output of each step is shorter. The steps still happen." color={C.coral} blocks={search} trim={0.4} start={70}/>
				<Lane
					frame={frame} y={880} title="+ PIXEL" tag="the brief arrives with the prompt: callers, tests, file:line" color={C.green}
					blocks={[{label: 'prompt + brief', out: 70, brief: true}, {label: 'answer', out: 0, answer: true}]} start={120}
				/>
			</svg>
			<Title lines={['PIXEL cuts turns.', 'RTK only trims output.']}/>
			<Foot text="Illustration of the mechanism, not a recording."/>
		</AbsoluteFill>
	);
};

type Stage = {when: string; head: string; lines: string[]; note: string};

const stages: Stage[] = [
	{when: 'BEFORE AN EDIT', head: '[PIXEL:EDIT]', lines: ['retry_limit(): shape changes', '3 proved callers:', 'client.rs:88  worker.rs:41', 'covered by retry_test.rs'], note: 'once per symbol, per session'},
	{when: 'AFTER THE EDIT', head: '[PIXEL:STALE-CALLERS]', lines: ['client.rs:88   updated', 'worker.rs:41   not yet', 'likely, not proved: job.rs:12'], note: 'only what is still stale'},
	{when: 'AT STOP', head: 'block, once', lines: ['update worker.rs:41,', 'or say why it is fine.', '', '(likely edges never block)'], note: 'the same set never blocks twice'},
];

/// The edit hooks in one picture. They ship switched off: whether they
/// remove turns on edit tasks is still an open comparison.
export const EditGuard = () => {
	const frame = useCurrentFrame();
	return (
		<AbsoluteFill style={{background: C.ground}}>
			<svg width={W} height={H} viewBox={`0 0 ${W} ${H}`} style={{position: 'absolute'}}>
				<rect width={W} height={H} fill={C.ground}/>
				<ellipse cx={W / 2} cy={H} rx={900} ry={330} fill={C.green} opacity={0.07}/>
				{stages.map((s, i) => {
					const t = rise(frame, 30 + i * 55, 20);
					const x = 80 + i * 500;
					return (
						<g key={s.when} transform={`translate(${x}, ${330 + (1 - t) * 18})`} opacity={t}>
							<rect width={440} height={470} rx={6} fill={C.term} stroke={i === 2 ? C.coral : C.green} strokeWidth={2}/>
							<text x={24} y={46} fontFamily={F.mono} fontSize={20} letterSpacing={3} fill={C.inkSoft}>{s.when}</text>
							<text x={24} y={100} fontFamily={F.display} fontSize={48} fontWeight={700} fill={i === 2 ? C.coralInk : C.greenHi} style={PX}>{s.head}</text>
							{s.lines.map((l, j) => (
								<text key={j} x={24} y={160 + j * 44} fontFamily={F.mono} fontSize={21} fill={C.ink} opacity={rise(frame, 40 + i * 55 + j * 6, 10)}>{l}</text>
							))}
							<text x={24} y={434} fontFamily={F.mono} fontSize={19} fill={C.inkSoft}>{s.note}</text>
						</g>
					);
				})}
			</svg>
			<Title lines={['Edit, check, stop.', 'Quiet in between.']}/>
			<Foot text="Edit hooks are off by default until an edit-task comparison shows they remove turns."/>
		</AbsoluteFill>
	);
};
