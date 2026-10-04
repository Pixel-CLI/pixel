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
		{x: setupXs[1], y: setupY, label: 'prepare-repo', sub: 'index + graph', icon: 'init' as const, local: true},
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
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'use pixel first', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'new session', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'Agent', sub: 'asks pixel', icon: 'agent'},
				{x: sessXs[3], y: sessY, label: 'find-code · impact', sub: 'bounded reads', icon: 'read', accent: GREEN, local: true},
				{x: sessXs[4], y: sessY, label: 'Precise Context', sub: 'no guessing', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'token-savings', sub: 'measured', icon: 'meter', accent: GREEN, local: true},
			],
		},
		loop: 'ONE .PIXEL DIR · EVERY AGENT · SAVINGS MEASURED, NOT CLAIMED',
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
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'impact before edit', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'rename the fn', icon: 'task'},
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
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'scope first', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'fix the bug', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'scope-task', sub: 'p0 · p1 · p2', icon: 'target', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: 'File List', sub: 'closed, ranked', icon: 'read'},
				{x: sessXs[4], y: sessY, label: 'Work P0', sub: 'then p1', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'Answered', sub: 'nothing wasted', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'ONLY THE FILES THAT MATTER · RANKED',
	},
};

export const rollbackSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'undo the bug', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'reconstructs', icon: 'agent'},
			{x: topXs[2], y: topY, label: 'Reflog Dig', sub: 'git log -p', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Guess Commit', sub: 'which one?', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Wrong Restore', sub: 'checkout x', icon: 'break'},
			{x: topXs[5], y: topY, label: 'More Damage', sub: 'new breakage', icon: 'guess'},
			{x: topXs[6], y: topY, label: 'Still Broken', sub: 'start over', icon: 'lost'},
		],
		loop: 'REPEATS EVERY ROLLBACK · GUESS WHAT BROKE',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['HISTORY IS', 'ALREADY', 'INDEXED'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'history indexed', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'undo the bug', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'plan-rollback', sub: 'locates the break', icon: 'clock', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: 'Last Good', sub: 'candidate flagged', icon: 'git'},
				{x: sessXs[4], y: sessY, label: 'Apply', sub: 'never resets', icon: 'shield'},
				{x: sessXs[5], y: sessY, label: 'Fixed', sub: 'evidence-backed', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'RESCUE IS A PLAN, NOT A DIG',
	},
};

export const publishSpec: ComparisonSpec = {
	width: 1600,
	height: 1000,
	accentA: RED,
	accentB: GREEN,
	dotColor: GREEN,
	panelA: {
		title: 'WITHOUT PIXEL',
		nodes: [
			{x: topXs[0], y: topY, label: 'Task', sub: 'ship the change', icon: 'task'},
			{x: topXs[1], y: topY, label: 'Agent', sub: 'git add -A', icon: 'edit'},
			{x: topXs[2], y: topY, label: 'Untracked Junk', sub: 'staged anyway', icon: 'search'},
			{x: topXs[3], y: topY, label: 'Secrets Staged', sub: '.env committed', icon: 'deadend', warn: true},
			{x: topXs[4], y: topY, label: 'Force Push', sub: '--force', icon: 'break'},
			{x: topXs[5], y: topY, label: 'History Mess', sub: 'merge commits', icon: 'guess'},
			{x: topXs[6], y: topY, label: 'Cleanup', sub: 'manual fix', icon: 'lost'},
		],
		loop: 'REPEATS EVERY PUSH · EVERY INCIDENT',
	},
	panelB: {
		title: 'WITH PIXEL',
		leftText: ['EVERY COMMIT', 'CRASH-SAFE', 'AND CHECKED'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Session Hook', sub: 'review first', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'ship the change', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'review-changes', sub: 'staged · untracked', icon: 'read', accent: GREEN, local: true},
				{x: sessXs[3], y: sessY, label: 'repo-state', sub: 'clean check', icon: 'context', local: true},
				{x: sessXs[4], y: sessY, label: 'commit-and-push', sub: 'idempotent', icon: 'rocket', local: true},
				{x: sessXs[5], y: sessY, label: 'Shipped', sub: 'leased push', icon: 'shield', accent: GREEN},
			],
		},
		loop: 'REVIEW BEFORE COMMIT · LEASED, NEVER FORCED',
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
		leftText: ['SAME COMMAND', 'INDEXED', 'ANSWER'],
		setup,
		session: {
			nodes: [
				{x: sessXs[0], y: sessY, label: 'Hook Rewrite', sub: 'transparent', icon: 'hook', accent: GREEN, local: true},
				{x: sessXs[1], y: sessY, label: 'Task', sub: 'find the call', icon: 'task'},
				{x: sessXs[2], y: sessY, label: 'rg needle', sub: 'same command', icon: 'agent'},
				{x: sessXs[3], y: sessY, label: 'search-content', sub: 'indexed, capped', icon: 'target', accent: GREEN, local: true},
				{x: sessXs[4], y: sessY, label: 'Ranked Hits', sub: 'with context', icon: 'context'},
				{x: sessXs[5], y: sessY, label: 'Answered', sub: 'first try', icon: 'ship', accent: GREEN},
			],
		},
		loop: 'SAME COMMAND · DETERMINISTIC ANSWER',
	},
};
