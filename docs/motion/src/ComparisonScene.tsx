// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {AbsoluteFill, useCurrentFrame} from 'remotion';
import {C, F, PX} from './theme';
import {Badge, Defs, dotX, halo, LocalMark, LoopArrow, Node, NodeSpec, Panel, Rail, RailProgress, TravelDot} from './parts';

export type ComparisonSpec = {
	width: number;
	height: number;
	accentA: string; // top rail / failure accent
	accentB: string; // pixel accent (panel tab, loop label)
	dotColor: string;
	panelA: {title: string; nodes: NodeSpec[]; loop: string};
	panelB: {
		title: string;
		leftText: string[];
		setup?: {nodes: NodeSpec[]; badge?: string};
		session: {nodes: NodeSpec[]};
		loop: string;
	};
};

const topY = 150;
const setupY = 520;
const sessY = 780;

const arrivals = (n: number, dur: number) =>
	Array.from({length: n}, (_, i) => (i * dur) / (n - 1));

/// Rounded elbow connecting the setup rail to the session rail, with
/// marching dashes like the loop arrows.
const Connector = ({x1, y1, x2, y2, color, frame}: {x1: number; y1: number; x2: number; y2: number; color: string; frame: number}) => (
	<path
		d={`M ${x1} ${y1} L ${x2 - 16} ${y1} Q ${x2} ${y1} ${x2} ${y1 + 16} L ${x2} ${y2 - 16} Q ${x2} ${y2} ${x2 + 16} ${y2}`}
		fill="none"
		stroke={color}
		strokeWidth={2.4}
		opacity={0.75}
		strokeDasharray="7 11"
		strokeDashoffset={-frame * 1.4}
		strokeLinecap="round"
	/>
);

export const ComparisonScene = ({spec}: {spec: ComparisonSpec}) => {
	const frame = useCurrentFrame();
	const {panelA, panelB} = spec;
	const topXs = panelA.nodes.map((n) => n.x);
	const sessXs = panelB.session.nodes.map((n) => n.x);
	const setupXs = panelB.setup?.nodes.map((n) => n.x) ?? [];

	const topDur = 240;
	const setupDur = setupXs.length ? (setupXs.length - 1) * 70 : 0;
	const sessDur = 240;

	const topArrivals = arrivals(topXs.length, topDur);
	const setupArrivals = setupXs.length ? arrivals(setupXs.length, setupDur) : [];
	const sessArrivals = arrivals(sessXs.length, sessDur);

	const lastSetup = panelB.setup?.nodes[setupXs.length - 1];

	return (
		<AbsoluteFill style={{background: C.ground}}>
			<svg width={spec.width} height={spec.height} viewBox={`0 0 ${spec.width} ${spec.height}`}>
				<Defs/>
				<rect width={spec.width} height={spec.height} fill={C.ground}/>
				<rect width={spec.width} height={spec.height} fill="url(#bgGlow)"/>
				<rect width={spec.width} height={spec.height} fill="url(#dotgrid)"/>
				{/* ambient glows tuned to each panel's accent */}
				<ellipse cx={spec.width / 2} cy={200} rx={880} ry={300} fill={spec.accentA} opacity={0.045} filter="url(#blur22)"/>
				<ellipse cx={spec.width / 2} cy={830} rx={880} ry={320} fill={spec.accentB} opacity={0.055} filter="url(#blur22)"/>

				{/* ── WITHOUT ── */}
				<Panel x={40} y={46} w={spec.width - 80} h={300} title={panelA.title} color={spec.accentA}/>
				<Rail nodes={panelA.nodes} color={C.coralDim}/>
				<RailProgress nodes={panelA.nodes} color={spec.accentA} x={dotX(frame, topXs, topDur)}/>
				<g transform={`translate(0, ${topY})`}>
					<TravelDot frame={frame} xs={topXs} duration={topDur} color={spec.accentA}/>
				</g>
				{panelA.nodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, topArrivals[i])}/>
				))}
				<LoopArrow
					x1={topXs[0]}
					x2={topXs[topXs.length - 1]}
					y={330}
					color={spec.accentA}
					label={panelA.loop}
				/>

				{/* ── WITH PIXEL ── */}
				<Panel x={40} y={400} w={spec.width - 80} h={spec.height - 40 - 400} title={panelB.title} color={spec.accentB} mark="pixel"/>

				{/* legend: what the CPU corner-mark means */}
				<g>
					<rect
						x={spec.width - 40 - 24 - 466}
						y={424}
						width={466}
						height={52}
						rx={4}
						fill={C.ground2}
						fillOpacity={0.85}
						stroke={spec.accentB}
						strokeOpacity={0.35}
						strokeWidth={1.4}
					/>
					<LocalMark x={spec.width - 40 - 24 - 466 + 14} y={438} size={24} color={spec.accentB}/>
					<text
						x={spec.width - 40 - 24 - 466 + 50}
						y={457}
						fontFamily={F.mono}
						fontSize={15}
						fontWeight={600}
						letterSpacing={1.4}
						fill={C.inkSoft}
					>
						RUNS LOCALLY · DETERMINISTIC · NO LLM
					</text>
				</g>

				{panelB.leftText.length > 0 && (
					<g>
						<rect x={104} y={490} width={5} height={130} rx={2.5} fill={spec.accentB} opacity={0.9}/>
						<text x={132} y={538} fontFamily={F.display} fontSize={48} fontWeight={700} style={PX} fill={C.inkFaint} letterSpacing={1}>
							{panelB.leftText.map((line, i) => (
								<tspan key={line} x={132} dy={i === 0 ? 0 : 46}>
									{line}
								</tspan>
							))}
						</text>
					</g>
				)}

				{panelB.setup && lastSetup && (
					<>
						<Rail nodes={panelB.setup.nodes} color={C.cellEdge}/>
						{frame <= setupDur && (
							<RailProgress nodes={panelB.setup.nodes} color={spec.dotColor} x={dotX(frame, setupXs, setupDur)}/>
						)}
						<Connector
							x1={lastSetup.x + 42}
							y1={setupY}
							x2={sessXs[0] + 50}
							y2={sessY - 42}
							color={C.cellEdge}
							frame={frame}
						/>
						{panelB.setup.badge && (
							<Badge x={sessXs[0] - 20} y={656} text={panelB.setup.badge} color={C.inkSoft}/>
						)}
						<g transform={`translate(0, ${setupY})`}>
							{frame <= setupDur && (
								<TravelDot frame={frame} xs={setupXs} duration={setupDur} color={spec.dotColor}/>
							)}
						</g>
						{panelB.setup.nodes.map((n, i) => (
							<Node key={n.label} spec={n} glow={halo(frame, setupArrivals[i])}/>
						))}
					</>
				)}

				<Rail nodes={panelB.session.nodes} color={C.cellEdge}/>
				<RailProgress nodes={panelB.session.nodes} color={spec.dotColor} x={dotX(frame, sessXs, sessDur)}/>
				<g transform={`translate(0, ${sessY})`}>
					<TravelDot frame={frame} xs={sessXs} duration={sessDur} color={spec.dotColor}/>
				</g>
				{panelB.session.nodes.map((n, i) => (
					<Node key={n.label} spec={n} glow={halo(frame, sessArrivals[i])}/>
				))}

				<LoopArrow
					x1={sessXs[sessXs.length - 1]}
					x2={sessXs[0]}
					y={930}
					color={spec.accentB}
					label={panelB.loop}
				/>
			</svg>
		</AbsoluteFill>
	);
};
