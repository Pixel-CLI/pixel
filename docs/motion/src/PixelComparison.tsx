// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {ComparisonSpec} from './ComparisonScene';
import {C} from './theme';

const RED = C.coral;
const GREEN = C.green;

const topY = 150;
const setupY = 520;
const sessY = 780;

const topXs = [140, 360, 580, 800, 1020, 1240, 1460];
const setupXs = [600, 830, 1060, 1290];
const sessXs = [1440, 1200, 960, 720, 480, 240];

const setup = {
	nodes: [
		{x: setupXs[0], y: setupY, label: 'Repo', sub: 'any language', icon: 'repo' as const},
		{x: setupXs[1], y: setupY, label: 'First Code Prompt', sub: 'indexes the repo', icon: 'init' as const, local: true},
		{x: setupXs[2], y: setupY, label: '.pixel/', sub: 'shards · call graph', icon: 'graph' as const},
		{x: setupXs[3], y: setupY, label: 'In Repo', sub: 'git-anchored', icon: 'git' as const},
	],
	badge: 'FRESH EVERY HEAD',
};

export const measuredSavingsSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'new session', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'zero memory', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'Blind Reads', sub: 'rg · cat · sed', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Dead Ends', sub: 'wrong files', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Guess Impact', sub: 're-derive', icon: 'guess'},
			{x: topXs[5], y: topY, label: 'Task Shipped', sub: 'budget spent', icon: 'ship'},
			{x: topXs[6], y: topY, label: 'Savings', sub: 'claimed, unmeasured', icon: 'lost'},
		],
		loop: "REPEATS EVERY SESSION · EVERY TEAMMATE'S AGENT",
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['INDEX ALREADY', 'KNOWS YOUR', 'REPO'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Task', sub: 'new session', icon: 'task'},
				{x: sessXs[1], y: sessY, label: 'pixel skill', sub: 'use pixel first', icon: 'hook', accent: GREEN},
				{x: sessXs[2], y: sessY, label: 'Agent', sub: 'asks pixel', icon: 'agent'},
				{x: sessXs[3], y: sessY, label: 'find-code · impact', sub: 'bounded reads', icon: 'read', accent: GREEN, local: true},
				{x: sessXs[4], y: sessY, label: 'Precise Context', sub: 'no guessing', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'pixel audit', sub: 'your largest files', icon: 'meter', accent: GREEN, local: true},
			],
		},
		loop: 'ONE .PIXEL DIR · EVERY AGENT · CHECK THE SAVINGS ON YOUR REPO',
	},
};

export const impactSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'rename the fn', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'edits blind', icon: 'edit'},
			{x: topXs[2], y: topY, label: 'Grep Callers', sub: 'partial hits', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Missed Sites', sub: 'dynamic · aliased', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Red CI', sub: 'broken callers', icon: 'break'},
			{x: topXs[5], y: topY, label: 'Rework', sub: 'round trips', icon: 'guess'},
			{x: topXs[6], y: topY, label: 'Trust', sub: 'eroded', icon: 'lost'},
		],
		loop: 'REPEATS EVERY REFACTOR · EVERY REPO',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['CALL GRAPH', 'ALREADY', 'RESOLVED'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Task', sub: 'rename the fn', icon: 'task'},
				{x: sessXs[1], y: sessY, label: 'pixel skill', sub: 'impact before edit', icon: 'hook', accent: GREEN},
				{x: sessXs[2], y: sessY, label: 'pixel impact', sub: 'callers upstream', icon: 'graph', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: 'Blast Radius', sub: 'every site listed', icon: 'context'},
				{x: sessXs[4], y: sessY, label: 'Edit', sub: 'informed', icon: 'edit'},
				{x: sessXs[5], y: sessY, label: 'Green CI', sub: 'verified', icon: 'shield', accent: GREEN},
			],
		},
		loop: 'IMPACT BEFORE EDIT · EVERY CALLER COUNTED',
	},
};

export const scopeSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'fix the bug', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'opens everything', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'Wander Files', sub: 'one by one', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Context Bloat', sub: 'irrelevant code', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Shallow Answer', sub: 'low signal', icon: 'guess'},
			{x: topXs[5], y: topY, label: 'Tokens Burned', sub: 'budget spent', icon: 'meter'},
			{x: topXs[6], y: topY, label: 'Again', sub: 'next task', icon: 'lost'},
		],
		loop: 'REPEATS EVERY TASK · EVERY FILE READ ANYWAY',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['SCOPE BEFORE', 'YOU READ', 'ANYTHING'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Task', sub: 'fix the bug', icon: 'task'},
				{x: sessXs[1], y: sessY, label: 'pixel skill', sub: 'scope first', icon: 'hook', accent: GREEN},
				{x: sessXs[2], y: sessY, label: 'scope-task', sub: 'p0 · p1 · p2', icon: 'target', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: 'File List', sub: 'closed, ranked', icon: 'read'},
				{x: sessXs[4], y: sessY, label: 'Work P0', sub: 'then p1', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'Answered', sub: 'nothing wasted', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'ONLY THE FILES THAT MATTER · RANKED',
	},
};

export const rewriteSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'find the call', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'runs rg', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'rg needle', sub: 'repo-wide', icon: 'search'},
			{x: topXs[3], y: topY, label: '10k Hits', sub: 'dumped raw', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Truncated', sub: 'context cap', icon: 'break'},
			{x: topXs[5], y: topY, label: 'Missed Match', sub: 'wrong answer', icon: 'guess'},
			{x: topXs[6], y: topY, label: 'Redo', sub: 'narrower query', icon: 'lost'},
		],
		loop: 'REPEATS EVERY SEARCH · RAW TEXT INTO CONTEXT',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['ONE INDEXED', 'SEARCH, NOT', 'TEN GREPS'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Task', sub: 'find the call', icon: 'task'},
				{x: sessXs[1], y: sessY, label: 'pixel skill', sub: 'search the index', icon: 'hook', accent: GREEN},
				{x: sessXs[2], y: sessY, label: 'Agent', sub: 'asks pixel', icon: 'agent'},
				{x: sessXs[3], y: sessY, label: 'search-content', sub: 'indexed, capped', icon: 'target', accent: GREEN, local: true},
				{x: sessXs[4], y: sessY, label: 'Capped Hits', sub: 'path:line', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'Answered', sub: 'first try', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'ONE INDEXED CALL · THEN A TARGETED READ',
	},
};

export const briefSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Prompt', sub: 'your question', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'plans a search', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'Grep', sub: 'rg · glob', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Read File', sub: '2,000 lines', icon: 'read'},
			{x: topXs[4], y: topY, label: 'Grep Again', sub: 'wrong match', icon: 'deadend', warn: true},
			{x: topXs[5], y: topY, label: 'Context Bloat', sub: 'unused code', icon: 'meter'},
			{x: topXs[6], y: topY, label: 'Late Answer', sub: 'budget spent', icon: 'clock'},
		],
		loop: 'PLAN · SEARCH · READ · REPEAT — EVERY PROMPT',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['EVIDENCE', 'BEFORE THE', 'FIRST TURN'],
		setup: {
			nodes: [
				{x: setupXs[0], y: setupY, label: 'pixel setup', sub: 'once', icon: 'init', local: true},
				{x: setupXs[1], y: setupY, label: 'Agent Hooks', sub: '7 hosts wired', icon: 'hook'},
				{x: setupXs[2], y: setupY, label: 'First Code Prompt', sub: 'indexes the repo', icon: 'repo', local: true},
				{x: setupXs[3], y: setupY, label: '.pixel/', sub: 'index · graph', icon: 'graph'},
			],
			badge: 'INDEXED ON FIRST CODE PROMPT',
		},
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'You', sub: 'ask as usual', icon: 'task'},
				{x: sessXs[1], y: sessY, label: 'Enter', sub: 'prompt hook', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[2], y: sessY, label: 'Local Index', sub: 'defs · callers · tests', icon: 'graph', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: '[PIXEL:BRIEF]', sub: 'cited file:line', icon: 'context', accent: GREEN},
				{x: sessXs[4], y: sessY, label: 'Agent', sub: 'evidence in context', icon: 'agent'},
				{x: sessXs[5], y: sessY, label: 'Answer', sub: 'right files first', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'CITED EVIDENCE WITH YOUR PROMPT · YOUR AGENT KEEPS ITS TOOLS',
	},
};
