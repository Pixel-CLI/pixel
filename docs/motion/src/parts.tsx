// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {Easing, useCurrentFrame} from 'remotion';
import type React from 'react';
import {C, F, PX} from './theme';

export type NodeSpec = {
	x: number;
	y: number;
	label: string;
	sub: string;
	icon: IconKind;
	accent?: string;
	warn?: boolean;
	local?: boolean;
};

export type IconKind =
	| 'task'
	| 'agent'
	| 'search'
	| 'deadend'
	| 'guess'
	| 'ship'
	| 'lost'
	| 'repo'
	| 'init'
	| 'graph'
	| 'git'
	| 'hook'
	| 'read'
	| 'context'
	| 'meter'
	| 'edit'
	| 'break'
	| 'shield'
	| 'clock'
	| 'target'
	| 'rocket';

export const TILE = 84;

/// Shared defs: tile gradients, dot-grid pattern, blur filters. Render once
/// inside the scene <svg>.
export const Defs = () => (
	<defs>
		<linearGradient id="tileGrad" x1="0" y1="0" x2="0" y2="1">
			<stop offset="0%" stopColor="#1c4a36"/>
			<stop offset="45%" stopColor="#153a2a"/>
			<stop offset="100%" stopColor="#0f2a1f"/>
		</linearGradient>
		<radialGradient id="tileSheen" cx="0.5" cy="0.05" r="1">
			<stop offset="0%" stopColor="#ffffff" stopOpacity={0.06}/>
			<stop offset="55%" stopColor="#ffffff" stopOpacity={0.025}/>
			<stop offset="100%" stopColor="#ffffff" stopOpacity={0}/>
		</radialGradient>
		<linearGradient id="warnGrad" x1="0" y1="0" x2="0" y2="1">
			<stop offset="0%" stopColor="#4a2319"/>
			<stop offset="100%" stopColor="#2a140e"/>
		</linearGradient>
		<radialGradient id="bgGlow" cx="0.5" cy="0.3" r="0.8">
			<stop offset="0%" stopColor="#14402c" stopOpacity={0.5}/>
			<stop offset="100%" stopColor={C.ground} stopOpacity={0}/>
		</radialGradient>
		<pattern id="dotgrid" width="32" height="32" patternUnits="userSpaceOnUse">
			<rect x="1" y="1" width="2" height="2" fill="#ffffff" opacity={0.045}/>
		</pattern>
		<filter id="blur6" x="-80%" y="-80%" width="260%" height="260%">
			<feGaussianBlur stdDeviation="6"/>
		</filter>
		<filter id="blur14" x="-80%" y="-80%" width="260%" height="260%">
			<feGaussianBlur stdDeviation="14"/>
		</filter>
		<filter id="blur22" x="-60%" y="-60%" width="220%" height="220%">
			<feGaussianBlur stdDeviation="22"/>
		</filter>
	</defs>
);

export const Icon = ({kind, color}: {kind: IconKind; color: string}) => {
	const s = {stroke: color, strokeWidth: 3.4, fill: 'none', strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const};
	switch (kind) {
		case 'task':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="6"/><line x1="21" y1="27" x2="43" y2="27"/><line x1="21" y1="35" x2="36" y2="35"/></g>);
		case 'agent':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="9" fill={color} stroke="none"/><ellipse cx="26" cy="32" rx="4" ry="5.5" fill={C.ground}/><ellipse cx="38" cy="32" rx="4" ry="5.5" fill={C.ground}/></g>);
		case 'search':
			return (<g {...s}><rect x="14" y="16" width="15" height="15" rx="3"/><rect x="35" y="16" width="15" height="15" rx="3"/><rect x="14" y="37" width="15" height="15" rx="3"/><circle cx="42" cy="42" r="9"/><line x1="48" y1="48" x2="55" y2="55"/></g>);
		case 'deadend':
			return (<g {...s}><rect x="15" y="16" width="22" height="30" rx="4"/><rect x="29" y="16" width="22" height="30" rx="4" transform="rotate(6 40 31)"/><line x1="22" y1="24" x2="32" y2="24"/></g>);
		case 'guess':
			return (<g {...s}><circle cx="22" cy="22" r="5"/><circle cx="44" cy="30" r="5"/><circle cx="26" cy="46" r="5"/><line x1="26" y1="25" x2="40" y2="28"/><line x1="41" y1="35" x2="30" y2="42"/><line x1="52" y1="14" x2="58" y2="8" strokeDasharray="1 6"/></g>);
		case 'ship':
			return (<g {...s}><rect x="16" y="18" width="30" height="30" rx="6"/><path d="M23 33 l6 6 l12 -13"/></g>);
		case 'lost':
			return (<g {...s}><rect x="16" y="14" width="32" height="38" rx="5"/><line x1="22" y1="22" x2="42" y2="22"/><line x1="22" y1="30" x2="42" y2="30"/><line x1="22" y1="38" x2="34" y2="38"/><circle cx="52" cy="16" r="2" stroke="none" fill={color}/><circle cx="58" cy="26" r="2" stroke="none" fill={color}/><circle cx="52" cy="36" r="2" stroke="none" fill={color}/></g>);
		case 'repo':
			return (<g {...s}><path d="M16 20 h14 l4 5 h14 v22 h-32 z"/><line x1="22" y1="34" x2="42" y2="34"/><line x1="22" y1="40" x2="36" y2="40"/></g>);
		case 'init':
			return (<g {...s}><circle cx="24" cy="22" r="5"/><circle cx="44" cy="30" r="5"/><circle cx="26" cy="46" r="5"/><line x1="28" y1="25" x2="40" y2="28"/><line x1="41" y1="35" x2="30" y2="43"/></g>);
		case 'graph':
			return (<g {...s}><rect x="14" y="18" width="36" height="28" rx="6"/><rect x="20" y="36" width="24" height="4" rx="2" fill={color} stroke="none"/><rect x="20" y="28" width="14" height="4" rx="2" fill={color} stroke="none"/></g>);
		case 'git':
			return (<g {...s}><line x1="32" y1="12" x2="32" y2="52"/><circle cx="32" cy="18" r="4"/><circle cx="32" cy="46" r="4"/><path d="M32 30 q12 2 16 -8"/><circle cx="48" cy="20" r="4"/></g>);
		case 'hook':
			return (<g {...s}><path d="M40 12 v18 a10 10 0 1 1 -16 0 v-4"/><path d="M24 22 l6 6"/><path d="M40 12 l5 5 M40 12 l-5 5" transform="translate(0 0)"/></g>);
		case 'read':
			return (<g {...s}><rect x="16" y="14" width="32" height="38" rx="5"/><line x1="22" y1="23" x2="42" y2="23"/><line x1="22" y1="31" x2="42" y2="31"/><line x1="22" y1="39" x2="36" y2="39"/><line x1="22" y1="47" x2="30" y2="47"/></g>);
		case 'context':
			return (<g {...s}><rect x="18" y="16" width="28" height="34" rx="5"/><rect x="23" y="22" width="18" height="6" rx="2" fill={color} stroke="none"/><line x1="23" y1="34" x2="41" y2="34"/><line x1="23" y1="40" x2="35" y2="40"/></g>);
		case 'meter':
			return (<g {...s}><path d="M18 46 a15 15 0 0 1 28 0"/><line x1="32" y1="42" x2="42" y2="28"/><circle cx="32" cy="44" r="3.5" fill={color} stroke="none"/></g>);
		case 'edit':
			return (<g {...s}><path d="M40 14 l10 10 -22 22 -12 2 2 -12 z"/><line x1="35" y1="19" x2="45" y2="29"/></g>);
		case 'break':
			return (<g {...s}><path d="M26 22 l-8 8 8 8"/><path d="M38 22 l8 8 -8 8"/><line x1="34" y1="16" x2="30" y2="46"/></g>);
		case 'shield':
			return (<g {...s}><path d="M32 12 l18 7 v12 c0 12 -8 19 -18 23 -10 -4 -18 -11 -18 -23 v-12 z"/><path d="M25 32 l5 5 10 -10"/></g>);
		case 'clock':
			return (<g {...s}><circle cx="32" cy="32" r="20"/><line x1="32" y1="20" x2="32" y2="32"/><line x1="32" y1="32" x2="42" y2="38"/><path d="M16 10 a26 26 0 0 1 34 -4" /><path d="M50 6 l0 8 -8 -2" fill="none"/></g>);
		case 'target':
			return (<g {...s}><circle cx="32" cy="32" r="20"/><circle cx="32" cy="32" r="12"/><circle cx="32" cy="32" r="4" fill={color} stroke="none"/></g>);
		case 'rocket':
			return (<g {...s}><path d="M32 10 c10 4 12 16 8 28 l-8 8 -8 -8 c-4 -12 -2 -24 8 -28 z"/><circle cx="32" cy="26" r="4"/><path d="M24 42 l-6 8 M40 42 l6 8"/></g>);
	}
};

/// The pixel logo: tapering "smile" curve ending in a rounded green square,
/// same shape as docs/pixel-line.svg.
export const PixelMark = ({x, y, scale = 1, color = '#22c55e', edge = '#16a34a'}: {x: number; y: number; scale?: number; color?: string; edge?: string}) => (
	<g transform={`translate(${x}, ${y}) scale(${scale})`}>
		<path d="M 0 4 Q 14 10 26 11" stroke={color} strokeWidth={3.4} strokeLinecap="round" fill="none"/>
		<path d="M 26 11 Q 36 12 44 10" stroke={color} strokeWidth={2.6} strokeLinecap="round" fill="none"/>
		<rect x={46} y={1} width={18} height={18} rx={4.5} fill={color} stroke={edge} strokeWidth={1.4}/>
	</g>
);

/// Corner marker: runs locally + deterministically, no LLM call. Drawn as a
/// mini CPU inside a small rounded chip — reused by Node and the legend.
export const LocalMark = ({x, y, size = 24, color}: {x: number; y: number; size?: number; color: string}) => {
	const u = size / 24;
	return (
		<g transform={`translate(${x}, ${y}) scale(${u})`}>
			<rect width={24} height={24} rx={3} fill={C.ground} stroke={color} strokeOpacity={0.85} strokeWidth={1.6}/>
			<g stroke={color} strokeWidth={1.5} fill="none" strokeLinecap="round">
				<rect x={7} y={7} width={10} height={10} rx={2}/>
				<line x1={10} y1={3.5} x2={10} y2={7}/><line x1={14} y1={3.5} x2={14} y2={7}/>
				<line x1={10} y1={17} x2={10} y2={20.5}/><line x1={14} y1={17} x2={14} y2={20.5}/>
				<line x1={3.5} y1={10} x2={7} y2={10}/><line x1={3.5} y1={14} x2={7} y2={14}/>
				<line x1={17} y1={10} x2={20.5} y2={10}/><line x1={17} y1={14} x2={20.5} y2={14}/>
			</g>
			<rect x={10.5} y={10.5} width={3} height={3} rx={0.8} fill={color}/>
		</g>
	);
};

/// One rail node: bloom + expanding halo ring + floating tile + label chip.
/// `glow` (0..1) is driven by the travelling dot's arrival time.
export const Node = ({spec, glow}: {spec: NodeSpec; glow: number}) => {
	const accent = spec.accent ?? C.inkSoft;
	const hot = spec.warn || !!spec.accent;
	const stroke = spec.warn ? C.coral : hot ? accent : C.cellEdge;
	const labelW = spec.label.length * 11.6;
	const subW = spec.sub.length * 9;
	const chipW = Math.max(labelW, subW) + 44;
	return (
		<g transform={`translate(${spec.x - TILE / 2}, ${spec.y - TILE / 2})`}>
			{/* ambient bloom + expanding arrival ring */}
			<circle cx={TILE / 2} cy={TILE / 2} r={TILE * 1.05} fill={accent} opacity={glow * 0.4} filter="url(#blur14)"/>
			<rect
				x={TILE / 2 - (TILE * 0.64 + (1 - glow) * 26)}
				y={TILE / 2 - (TILE * 0.64 + (1 - glow) * 26)}
				width={2 * (TILE * 0.64 + (1 - glow) * 26)}
				height={2 * (TILE * 0.64 + (1 - glow) * 26)}
				rx={6}
				fill="none"
				stroke={accent}
				strokeWidth={2}
				opacity={glow * 0.55}
			/>
			{/* drop shadow + tile */}
			<rect x={6} y={10} width={TILE - 12} height={TILE - 4} rx={6} fill="#000000" opacity={0.45} filter="url(#blur6)"/>
			<rect
				width={TILE}
				height={TILE}
				rx={6}
				fill={spec.warn ? 'url(#warnGrad)' : 'url(#tileGrad)'}
				stroke={stroke}
				strokeWidth={hot ? 2.2 : 1.4}
				strokeOpacity={hot ? 0.5 + glow * 0.5 : 0.9}
			/>
			<rect width={TILE} height={TILE} rx={6} fill="url(#tileSheen)"/>
			<line x1={22} y1={13} x2={TILE - 22} y2={13} stroke="#ffffff" strokeOpacity={0.1} strokeWidth={2} strokeLinecap="round"/>
			<g transform="translate(10,10)">
				<Icon kind={spec.icon} color={spec.warn ? C.coral : accent}/>
			</g>
			{spec.local && <LocalMark x={TILE - 14} y={-7} size={26} color={accent}/>}
			{/* label chip */}
			<rect
				x={TILE / 2 - chipW / 2}
				y={TILE + 10}
				width={chipW}
				height={64}
				rx={4}
				fill={C.ground}
				fillOpacity={0.7}
				stroke="#ffffff"
				strokeOpacity={0.05}
			/>
			<text x={TILE / 2} y={TILE + 37} textAnchor="middle" fontFamily={F.display} fontSize={28} fontWeight={700} letterSpacing={0.4} style={PX} fill={spec.warn ? C.coralInk : C.ink}>
				{spec.label}
			</text>
			<text x={TILE / 2} y={TILE + 59} textAnchor="middle" fontFamily={F.mono} fontSize={13} fontWeight={500} letterSpacing={1.2} fill={spec.warn ? '#d99a88' : C.inkSoft}>
				{spec.sub.toUpperCase()}
			</text>
		</g>
	);
};

export const railBounds = (nodes: NodeSpec[]) => {
	const y = nodes[0].y;
	const x1 = Math.min(...nodes.map((n) => n.x)) + TILE / 2;
	const x2 = Math.max(...nodes.map((n) => n.x)) - TILE / 2;
	return {x1, x2, y};
};

/// Baseline rail: soft under-glow + a gradient line that fades out at both ends.
export const Rail = ({nodes, color}: {nodes: NodeSpec[]; color: string}) => {
	const {x1, x2, y} = railBounds(nodes);
	// Keyed by geometry as well as colour: two rails of one colour (setup and
	// session) would otherwise share an id, and `url(#id)` resolves to the first,
	// fading the second rail over the first one's x-span.
	const gid = `railfade-${color.replace('#', '')}-${Math.round(x1)}-${Math.round(x2)}-${Math.round(y)}`;
	return (
		<g>
			<defs>
				<linearGradient id={gid} gradientUnits="userSpaceOnUse" x1={x1} y1={y} x2={x2} y2={y}>
					<stop offset={0} stopColor={color} stopOpacity={0}/>
					<stop offset={0.06} stopColor={color} stopOpacity={0.55}/>
					<stop offset={0.94} stopColor={color} stopOpacity={0.55}/>
					<stop offset={1} stopColor={color} stopOpacity={0}/>
				</linearGradient>
			</defs>
			<line x1={x1} y1={y} x2={x2} y2={y} stroke={color} strokeWidth={10} opacity={0.1} strokeLinecap="round" filter="url(#blur6)"/>
			<line x1={x1} y1={y} x2={x2} y2={y} stroke={`url(#${gid})`} strokeWidth={2.6} strokeLinecap="round"/>
		</g>
	);
};

/// Bright segment that trails the dot — the rail "lights up" behind it.
export const RailProgress = ({nodes, color, x}: {nodes: NodeSpec[]; color: string; x: number}) => {
	const {x1, x2, y} = railBounds(nodes);
	const xc = Math.max(x1, Math.min(x, x2));
	if (xc <= x1) return null;
	return (
		<g>
			<line x1={x1} y1={y} x2={xc} y2={y} stroke={color} strokeWidth={5} opacity={0.35} strokeLinecap="round" filter="url(#blur6)"/>
			<line x1={x1} y1={y} x2={xc} y2={y} stroke={color} strokeWidth={2.8} opacity={0.95} strokeLinecap="round"/>
		</g>
	);
};

const ease = Easing.inOut(Easing.quad);

export const dotX = (frame: number, xs: number[], duration: number) => {
	const seg = duration / (xs.length - 1);
	const i = Math.min(Math.floor(frame / seg), xs.length - 2);
	const t = Math.min(1, Math.max(0, ease((frame - i * seg) / seg)));
	return xs[i] + (xs[i + 1] - xs[i]) * t;
};

/// A square centred on (cx, cy) with half-side r: the pixel unit.
const Sq = ({cx, cy = 0, r, ...rest}: {cx: number; cy?: number; r: number} & React.SVGProps<SVGRectElement>) => (
	<rect x={cx - r} y={cy - r} width={2 * r} height={2 * r} rx={Math.min(2, r / 3)} {...rest}/>
);

/// Comet: bloom + white core + a decaying 5-pixel tail.
export const TravelDot = ({frame, xs, duration, color}: {frame: number; xs: number[]; duration: number; color: string}) => {
	const x = dotX(frame, xs, duration);
	const steps = [3, 7, 11, 16, 22];
	const radii = [9, 7.5, 6, 4.5, 3];
	const alphas = [0.3, 0.22, 0.15, 0.09, 0.05];
	return (
		<g>
			{steps.map((dt, i) => (
				<Sq key={dt} cx={dotX(Math.max(0, frame - dt), xs, duration)} r={radii[i]} fill={color} opacity={alphas[i]}/>
			))}
			<Sq cx={x} r={30} fill={color} opacity={0.22} filter="url(#blur14)"/>
			<Sq cx={x} r={13} fill={color} opacity={0.35} filter="url(#blur6)"/>
			<Sq cx={x} r={9.5} fill={color}/>
			<Sq cx={x} r={4} fill="#ffffff"/>
		</g>
	);
};

export const halo = (frame: number, arrival: number, span = 28) => {
	const d = Math.abs(frame - arrival);
	const t = Math.min(1, Math.max(0, d / span));
	return 1 - t;
};

/// Rounded-corner loop U with marching dashes and a status-dot pill label.
export const LoopArrow = ({x1, x2, y, color, label}: {x1: number; x2: number; y: number; color: string; label: string}) => {
	const frame = useCurrentFrame();
	const tw = label.length * 10.4;
	const w = tw + 62;
	const cx = (x1 + x2) / 2;
	return (
		<g>
			<path
				d={`M ${x2} ${y - 58} L ${x2} ${y - 16} Q ${x2} ${y} ${x2 - 16} ${y} L ${x1 + 16} ${y} Q ${x1} ${y} ${x1} ${y - 16} L ${x1} ${y - 58}`}
				fill="none"
				stroke={color}
				strokeWidth={2.4}
				opacity={0.85}
				strokeDasharray="7 11"
				strokeDashoffset={-frame * 1.4}
				strokeLinecap="round"
			/>
			<path d={`M ${x1} ${y - 80} l -9 16 h 18 z`} fill={color} opacity={0.95}/>
			<rect x={cx - w / 2 + 4} y={y - 14} width={w - 8} height={30} rx={4} fill="#000000" opacity={0.4} filter="url(#blur6)"/>
			<rect x={cx - w / 2} y={y - 19} width={w} height={38} rx={4} fill={C.ground2} fillOpacity={0.92} stroke={color} strokeOpacity={0.5} strokeWidth={1.4}/>
			<Sq cx={cx - tw / 2 - 14} cy={y} r={4.5} fill={color}/>
			<text x={cx + 8} y={y + 5.5} textAnchor="middle" fontFamily={F.mono} fontSize={15} fontWeight={600} letterSpacing={1.4} fill={color}>
				{label}
			</text>
		</g>
	);
};

/// Glass card with soft shadow, gradient rim and a glowing title tab.
export const Panel = ({x, y, w, h, title, color, mark}: {x: number; y: number; w: number; h: number; title: string; color: string; mark?: 'dot' | 'pixel'}) => {
	const textX = mark === 'pixel' ? x + 34 + 58 : x + 34 + 41;
	const tabW = title.length * 14.5 + (mark === 'pixel' ? 92 : 74);
	return (
		<g>
			<rect x={x + 10} y={y + 16} width={w - 20} height={h - 8} rx={10} fill="#000000" opacity={0.35} filter="url(#blur22)"/>
			<rect x={x} y={y} width={w} height={h} rx={10} fill={C.ground2} fillOpacity={0.8} stroke={color} strokeOpacity={0.28} strokeWidth={1.6}/>
			<rect x={x + 1} y={y + 1} width={w - 2} height={h - 2} rx={9} fill="none" stroke="#ffffff" strokeOpacity={0.04}/>
			<rect x={x + 34} y={y - 22} width={tabW} height={46} rx={4} fill={color} opacity={0.32} filter="url(#blur14)"/>
			<rect x={x + 34} y={y - 22} width={tabW} height={46} rx={4} fill={color}/>
			{mark === 'pixel' ? (
				<rect x={x + 34 + 18} y={y - 8} width={18} height={18} rx={3} fill={C.onGreen}/>
			) : (
				<Sq cx={x + 34 + 25} cy={y + 1} r={5.5} fill={C.onGreen}/>
			)}
			<text x={textX} y={y + 11} fontFamily={F.display} fontSize={32} fontWeight={700} letterSpacing={1.2} style={PX} fill={C.onGreen}>
				{title}
			</text>
		</g>
	);
};

export const Badge = ({x, y, text, color}: {x: number; y: number; text: string; color: string}) => {
	const tw = text.length * 9.6;
	const w = tw + 56;
	return (
		<g>
			<rect x={x - w / 2} y={y - 17} width={w} height={34} rx={4} fill={C.ground2} fillOpacity={0.92} stroke={color} strokeOpacity={0.5} strokeWidth={1.4}/>
			<Sq cx={x - tw / 2 - 12} cy={y} r={4} fill={color}/>
			<text x={x + 8} y={y + 5} textAnchor="middle" fontFamily={F.mono} fontSize={14} fontWeight={600} letterSpacing={1.2} fill={color}>
				{text}
			</text>
		</g>
	);
};

export const useFrame = useCurrentFrame;
