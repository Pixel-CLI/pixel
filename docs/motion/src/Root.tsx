// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import {Composition} from 'remotion';
import {AgentDemo, DemoProps, demoFrames, FPS, Trace} from './AgentDemo';
import {ComparisonScene, ComparisonSpec} from './ComparisonScene';
import pixelRun from './demo/pixel.json';
import vanillaRun from './demo/vanilla.json';
import {impactSpec, measuredSavingsSpec, publishSpec, rollbackSpec, rewriteSpec, scopeSpec} from './PixelComparison';

const comps: [string, ComparisonSpec][] = [
	['PixelComparison', measuredSavingsSpec],
	['PixelImpact', impactSpec],
	['PixelScope', scopeSpec],
	['PixelRollback', rollbackSpec],
	['PixelPublish', publishSpec],
	['PixelRewrite', rewriteSpec],
];

// The recorded runs are 1 to 2 minutes long: replay them at a fixed speed
// on one shared clock, so the gap between the two stays true to scale.
const demo: DemoProps = {
	vanilla: vanillaRun as Trace,
	pixel: pixelRun as Trace,
	speed: 6,
	recorded: '2026-09-23',
	modelName: 'Claude Sonnet 5',
};

export const RemotionRoot = () => (
	<>
		<Composition
			id="AgentDemo"
			component={AgentDemo}
			durationInFrames={demoFrames(demo.vanilla, demo.pixel, demo.speed)}
			fps={FPS}
			width={1600}
			height={1000}
			defaultProps={demo}
		/>
		{comps.map(([id, spec]) => (
			<Composition
				key={id}
				id={id}
				component={ComparisonScene}
				durationInFrames={240}
				fps={30}
				width={1600}
				height={1000}
				defaultProps={{spec}}
			/>
		))}
	</>
);
