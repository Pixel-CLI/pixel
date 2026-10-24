// SPDX-FileCopyrightText: The RTO contributors
// SPDX-License-Identifier: MIT

// The website's identity (website/assets/css/main.css), so the animations
// read as part of the same page: navy ground, orange for the cost that still
// happens (a turn, a trimmed log), indigo for what PIXEL hands back, Handjet
// for display text. The key names keep their original spelling: `green` is
// the indigo accent and `coral` the orange.
export const C = {
	ground: '#0b1020',
	ground2: '#101730',
	panel: '#172046',
	ink: '#eef1ff',
	inkSoft: '#a3acd1',
	inkFaint: '#4b5688',
	line: '#263068',
	cell: '#1a2350',
	cellEdge: '#2b3875',
	green: '#818cf8',
	greenHi: '#a5b4fc',
	greenDim: '#4f55c9',
	coral: '#fb923c',
	coralDim: '#7a4a22',
	coralInk: '#fdba74',
	onGreen: '#0a0d29',
	term: '#070b18',
};

export const F = {
	display: '"Handjet", "Archivo", sans-serif',
	sans: '"Archivo", sans-serif',
	mono: '"IBM Plex Mono", monospace',
};

/// Handjet's square elements, as the site sets them.
export const PX = {fontVariationSettings: '"ELSH" 2, "ELGR" 1.5'} as const;
