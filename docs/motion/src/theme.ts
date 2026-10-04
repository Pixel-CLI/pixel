// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// The website's identity (website/assets/css/main.css), so the animations
// read as part of the same page: forest-green ground, coral for what an
// agent wastes, green for what Pixel hands back, Handjet for display text.
export const C = {
	ground: '#0b1f17',
	ground2: '#0f281d',
	panel: '#133426',
	ink: '#ecf7ef',
	inkSoft: '#93b3a0',
	inkFaint: '#3d6b54',
	line: '#1f4535',
	cell: '#173b2d',
	cellEdge: '#22503d',
	green: '#22c55e',
	greenHi: '#4ade80',
	greenDim: '#2f7a4f',
	coral: '#f0775a',
	coralDim: '#6b3a2e',
	coralInk: '#ffb4a1',
	onGreen: '#04210e',
	term: '#07160f',
};

export const F = {
	display: '"Handjet", "Archivo", sans-serif',
	sans: '"Archivo", sans-serif',
	mono: '"IBM Plex Mono", monospace',
};

/// Handjet's square elements, as the site sets them.
export const PX = {fontVariationSettings: '"ELSH" 2, "ELGR" 1.5'} as const;
